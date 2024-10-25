use crate::avro::de;
use crate::proto::schema::get_pool;
use crate::{proto, should_flush};
use arrow::array::{Int32Builder, Int64Builder};
use arrow::compute::kernels;
use arrow_array::builder::{
    ArrayBuilder, GenericByteBuilder, StringBuilder, TimestampNanosecondBuilder,
};
use arrow_array::types::GenericBinaryType;
use arrow_array::RecordBatch;
use arroyo_rpc::df::ArroyoSchema;
use arroyo_rpc::formats::{
    AvroFormat, BadData, Format, Framing, FramingMethod, JsonFormat, ProtobufFormat,
};
use arroyo_rpc::schema_resolver::{FailingSchemaResolver, FixedSchemaResolver, SchemaResolver};
use arroyo_types::{to_nanos, SourceError};
use prost_reflect::DescriptorPool;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::sync::Mutex;

pub struct FramingIterator<'a> {
    framing: Option<Arc<Framing>>,
    buf: &'a [u8],
    offset: usize,
}

impl<'a> FramingIterator<'a> {
    pub fn new(framing: Option<Arc<Framing>>, buf: &'a [u8]) -> Self {
        Self {
            framing,
            buf,
            offset: 0,
        }
    }
}

impl<'a> Iterator for FramingIterator<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.buf.len() {
            return None;
        }

        match &self.framing {
            Some(framing) => {
                match &framing.method {
                    FramingMethod::Newline(newline) => {
                        let end = memchr::memchr(b'\n', &self.buf[self.offset..])
                            .map(|i| self.offset + i)
                            .unwrap_or(self.buf.len());

                        let prev = self.offset;
                        self.offset = end + 1;

                        // enforce max len if set
                        let length =
                            (end - prev).min(newline.max_line_length.unwrap_or(u64::MAX) as usize);

                        Some(&self.buf[prev..(prev + length)])
                    }
                }
            }
            None => {
                self.offset = self.buf.len();
                Some(self.buf)
            }
        }
    }
}

pub struct ArrowDeserializer {
    format: Arc<Format>,
    framing: Option<Arc<Framing>>,
    schema: ArroyoSchema,
    bad_data: BadData,
    json_decoder: Option<(arrow::json::reader::Decoder, TimestampNanosecondBuilder)>,
    buffered_count: usize,
    buffered_since: Instant,
    schema_registry: Arc<Mutex<HashMap<u32, apache_avro::schema::Schema>>>,
    proto_pool: DescriptorPool,
    schema_resolver: Arc<dyn SchemaResolver + Sync>,
    kafka_metadata_builder: Option<(Int64Builder, Int32Builder, StringBuilder)>,
}

impl ArrowDeserializer {
    pub fn new(
        format: Format,
        schema: ArroyoSchema,
        framing: Option<Framing>,
        bad_data: BadData,
    ) -> Self {
        let resolver = if let Format::Avro(AvroFormat {
            reader_schema: Some(schema),
            ..
        }) = &format
        {
            Arc::new(FixedSchemaResolver::new(0, schema.clone().into()))
                as Arc<dyn SchemaResolver + Sync>
        } else {
            Arc::new(FailingSchemaResolver::new()) as Arc<dyn SchemaResolver + Sync>
        };

        Self::with_schema_resolver(format, framing, schema, bad_data, resolver)
    }

    pub fn with_schema_resolver(
        format: Format,
        framing: Option<Framing>,
        schema: ArroyoSchema,
        bad_data: BadData,
        schema_resolver: Arc<dyn SchemaResolver + Sync>,
    ) -> Self {
        let proto_pool = if let Format::Protobuf(ProtobufFormat {
            compiled_schema: Some(schema),
            ..
        }) = &format
        {
            get_pool(schema).expect("unable to handle protobuf schema")
        } else {
            DescriptorPool::global()
        };

        Self {
            json_decoder: matches!(
                format,
                Format::Json(..)
                    | Format::Avro(AvroFormat {
                        into_unstructured_json: false,
                        ..
                    })
                    | Format::Protobuf(ProtobufFormat {
                        into_unstructured_json: false,
                        ..
                    })
            )
            .then(|| {
                // exclude the timestamp field
                (
                    arrow_json::reader::ReaderBuilder::new(Arc::new(
                        schema.schema_without_timestamp(),
                    ))
                    .with_limit_to_batch_size(false)
                    .with_strict_mode(false)
                    .with_allow_bad_data(matches!(bad_data, BadData::Drop { .. }))
                    .build_decoder()
                    .unwrap(),
                    TimestampNanosecondBuilder::new(),
                )
            }),
            format: Arc::new(format),
            framing: framing.map(Arc::new),
            schema,
            schema_registry: Arc::new(Mutex::new(HashMap::new())),
            bad_data,
            schema_resolver,
            proto_pool,
            buffered_count: 0,
            buffered_since: Instant::now(),
            kafka_metadata_builder: None,
        }
    }

    pub async fn deserialize_slice(
        &mut self,
        buffer: &mut [Box<dyn ArrayBuilder>],
        msg: &[u8],
        timestamp: SystemTime,
        kafka_metadata: (bool, i64, i32, String),
    ) -> Vec<SourceError> {
        match &*self.format {
            Format::Avro(_) => self.deserialize_slice_avro(buffer, msg, timestamp).await,
            _ => FramingIterator::new(self.framing.clone(), msg)
                .map(|t| self.deserialize_single(buffer, t, timestamp, kafka_metadata.clone()))
                .filter_map(|t| t.err())
                .collect(),
        }
    }

    pub fn should_flush(&self) -> bool {
        should_flush(self.buffered_count, self.buffered_since)
    }

    pub fn flush_buffer(&mut self) -> Option<Result<RecordBatch, SourceError>> {
        let (decoder, timestamp) = self.json_decoder.as_mut()?;
        self.buffered_since = Instant::now();
        self.buffered_count = 0;
        match self.bad_data {
            BadData::Fail { .. } => Some(
                decoder
                    .flush()
                    .map_err(|e| {
                        SourceError::bad_data(format!("JSON does not match schema: {:?}", e))
                    })
                    .transpose()?
                    .map(|batch| {
                        let mut columns = batch.columns().to_vec();
                        columns.insert(self.schema.timestamp_index, Arc::new(timestamp.finish()));

                        if let Some((offset_builder, partition_builder, topic_builder)) =
                            &mut self.kafka_metadata_builder
                        {
                            if let Some((topic_idx, _)) =
                                self.schema.schema.column_with_name("topic")
                            {
                                columns.remove(topic_idx);
                                columns.insert(topic_idx, Arc::new(topic_builder.finish()));
                            }

                            if let Some((partition_idx, _)) =
                                self.schema.schema.column_with_name("partition")
                            {
                                columns.remove(partition_idx);
                                columns.insert(partition_idx, Arc::new(partition_builder.finish()));
                            }

                            if let Some((offset_idx, _)) =
                                self.schema.schema.column_with_name("offset")
                            {
                                columns.remove(offset_idx);
                                columns.insert(offset_idx, Arc::new(offset_builder.finish()));
                            }
                        }
                        RecordBatch::try_new(self.schema.schema.clone(), columns).unwrap()
                    }),
            ),
            BadData::Drop { .. } => Some(
                decoder
                    .flush_with_bad_data()
                    .map_err(|e| {
                        SourceError::bad_data(format!(
                            "Something went wrong decoding JSON: {:?}",
                            e
                        ))
                    })
                    .transpose()?
                    .map(|(batch, mask, _)| {
                        let mut columns = batch.columns().to_vec();
                        let timestamp =
                            kernels::filter::filter(&timestamp.finish(), &mask).unwrap();

                        columns.insert(self.schema.timestamp_index, Arc::new(timestamp));
                        RecordBatch::try_new(self.schema.schema.clone(), columns).unwrap()
                    }),
            ),
        }
    }

    fn deserialize_single(
        &mut self,
        buffer: &mut [Box<dyn ArrayBuilder>],
        msg: &[u8],
        timestamp: SystemTime,
        kafka_metadata: (bool, i64, i32, String),
    ) -> Result<(), SourceError> {
        match &*self.format {
            Format::RawString(_)
            | Format::Json(JsonFormat {
                unstructured: true, ..
            }) => {
                self.deserialize_raw_string(buffer, msg);
                add_timestamp(buffer, self.schema.timestamp_index, timestamp);
                if kafka_metadata.0 {
                    add_kafka_metadata(
                        buffer,
                        &self.schema,
                        &kafka_metadata.3,
                        kafka_metadata.2,
                        kafka_metadata.1,
                    );
                }
            }
            Format::RawBytes(_) => {
                self.deserialize_raw_bytes(buffer, msg);
                add_timestamp(buffer, self.schema.timestamp_index, timestamp);
                if kafka_metadata.0 {
                    add_kafka_metadata(
                        buffer,
                        &self.schema,
                        &kafka_metadata.3,
                        kafka_metadata.2,
                        kafka_metadata.1,
                    );
                }
            }
            Format::Json(json) => {
                let msg = if json.confluent_schema_registry {
                    &msg[5..]
                } else {
                    msg
                };

                let Some((decoder, timestamp_builder)) = &mut self.json_decoder else {
                    panic!("json decoder not initialized");
                };

                if kafka_metadata.0 {
                    self.kafka_metadata_builder.get_or_insert_with(|| {
                        (
                            Int64Builder::new(),
                            Int32Builder::new(),
                            StringBuilder::new(),
                        )
                    });
                }

                decoder
                    .decode(msg)
                    .map_err(|e| SourceError::bad_data(format!("invalid JSON: {:?}", e)))?;
                timestamp_builder.append_value(to_nanos(timestamp) as i64);
                if kafka_metadata.0 {
                    if let Some((offset_builder, partition_builder, topic_builder)) =
                        &mut self.kafka_metadata_builder
                    {
                        offset_builder.append_value(kafka_metadata.1);
                        partition_builder.append_value(kafka_metadata.2);
                        topic_builder.append_value(kafka_metadata.3.clone());
                    }
                }
                self.buffered_count += 1;
            }
            Format::Protobuf(proto) => {
                let json = proto::de::deserialize_proto(&mut self.proto_pool, proto, msg)?;

                if proto.into_unstructured_json {
                    self.decode_into_json(buffer, json, timestamp);
                } else {
                    let Some((decoder, timestamp_builder)) = &mut self.json_decoder else {
                        panic!("json decoder not initialized");
                    };

                    decoder
                        .decode(json.to_string().as_bytes())
                        .map_err(|e| SourceError::bad_data(format!("invalid JSON: {:?}", e)))?;
                    timestamp_builder.append_value(to_nanos(timestamp) as i64);
                    if kafka_metadata.0 {
                        add_kafka_metadata(
                            buffer,
                            &self.schema,
                            &kafka_metadata.3,
                            kafka_metadata.2,
                            kafka_metadata.1,
                        );
                    }
                    self.buffered_count += 1;
                }
            }
            Format::Avro(_) => unreachable!("this should not be called for avro"),
            Format::Parquet(_) => todo!("parquet is not supported as an input format"),
        }

        Ok(())
    }

    fn decode_into_json(
        &mut self,
        builders: &mut [Box<dyn ArrayBuilder>],
        value: Value,
        timestamp: SystemTime,
    ) {
        let (idx, _) = self
            .schema
            .schema
            .column_with_name("value")
            .expect("no 'value' column for unstructured avro");
        let array = builders[idx]
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .expect("'value' column has incorrect type");

        array.append_value(value.to_string());
        add_timestamp(builders, self.schema.timestamp_index, timestamp);
        self.buffered_count += 1;
    }

    pub async fn deserialize_slice_avro<'a>(
        &mut self,
        builders: &mut [Box<dyn ArrayBuilder>],
        msg: &'a [u8],
        timestamp: SystemTime,
    ) -> Vec<SourceError> {
        let Format::Avro(format) = &*self.format else {
            unreachable!("not avro");
        };

        let messages = match de::avro_messages(
            format,
            &self.schema_registry,
            &self.schema_resolver,
            msg,
        )
        .await
        {
            Ok(messages) => messages,
            Err(e) => {
                return vec![e];
            }
        };

        let into_json = format.into_unstructured_json;

        messages
            .into_iter()
            .map(|record| {
                let value = record.map_err(|e| {
                    SourceError::bad_data(format!("failed to deserialize from avro: {:?}", e))
                })?;

                if into_json {
                    self.decode_into_json(builders, de::avro_to_json(value), timestamp);
                } else {
                    // for now round-trip through json in order to handle unsupported avro features
                    // as that allows us to rely on raw json deserialization
                    let json = de::avro_to_json(value).to_string();

                    let Some((decoder, timestamp_builder)) = &mut self.json_decoder else {
                        panic!("json decoder not initialized");
                    };

                    decoder
                        .decode(json.as_bytes())
                        .map_err(|e| SourceError::bad_data(format!("invalid JSON: {:?}", e)))?;
                    self.buffered_count += 1;
                    timestamp_builder.append_value(to_nanos(timestamp) as i64);
                }

                Ok(())
            })
            .filter_map(|r: Result<(), SourceError>| r.err())
            .collect()
    }

    fn deserialize_raw_string(&mut self, buffer: &mut [Box<dyn ArrayBuilder>], msg: &[u8]) {
        let (col, _) = self
            .schema
            .schema
            .column_with_name("value")
            .expect("no 'value' column for RawString format");
        buffer[col]
            .as_any_mut()
            .downcast_mut::<StringBuilder>()
            .expect("'value' column has incorrect type")
            .append_value(String::from_utf8_lossy(msg));
    }

    fn deserialize_raw_bytes(&mut self, buffer: &mut [Box<dyn ArrayBuilder>], msg: &[u8]) {
        let (col, _) = self
            .schema
            .schema
            .column_with_name("value")
            .expect("no 'value' column for RawBytes format");
        buffer[col]
            .as_any_mut()
            .downcast_mut::<GenericByteBuilder<GenericBinaryType<i32>>>()
            .expect("'value' column has incorrect type")
            .append_value(msg);
    }

    pub fn bad_data(&self) -> &BadData {
        &self.bad_data
    }
}

pub(crate) fn add_timestamp(
    builder: &mut [Box<dyn ArrayBuilder>],
    idx: usize,
    timestamp: SystemTime,
) {
    builder[idx]
        .as_any_mut()
        .downcast_mut::<TimestampNanosecondBuilder>()
        .expect("_timestamp column has incorrect type")
        .append_value(to_nanos(timestamp) as i64);
}

pub(crate) fn add_kafka_metadata_partition(
    builder: &mut [Box<dyn ArrayBuilder>],
    idx: usize,
    partition: i32,
) {
    builder[idx]
        .as_any_mut()
        .downcast_mut::<arrow::array::Int32Builder>()
        .expect("kafka_metadata.partition column has incorrect type")
        .append_value(partition);
}

pub(crate) fn add_kafka_metadata_offset(
    builder: &mut [Box<dyn ArrayBuilder>],
    idx: usize,
    offset: i64,
) {
    builder[idx]
        .as_any_mut()
        .downcast_mut::<Int64Builder>()
        .expect("kafka_metadata.offset column has incorrect type")
        .append_value(offset);
}

pub(crate) fn add_kafka_metadata_topic(
    builder: &mut [Box<dyn ArrayBuilder>],
    idx: usize,
    topic: &str,
) {
    builder[idx]
        .as_any_mut()
        .downcast_mut::<StringBuilder>()
        .expect("kafka_metadata.topic column has incorrect type")
        .append_value(topic)
}

pub(crate) fn add_kafka_metadata(
    builder: &mut [Box<dyn ArrayBuilder>],
    schema: &ArroyoSchema,
    topic: &str,
    partition: i32,
    offset: i64,
) {
    let (topic_idx, _) = schema
        .schema
        .column_with_name("topic")
        .expect("No column named 'topic' in the schema");
    let (partition_idx, _) = schema
        .schema
        .column_with_name("partition")
        .expect("No column named 'partition' in the schema");
    let (offset_idx, _) = schema
        .schema
        .column_with_name("offset")
        .expect("No column named 'offset' in the schema");
    add_kafka_metadata_topic(builder, topic_idx, topic);
    add_kafka_metadata_partition(builder, partition_idx, partition);
    add_kafka_metadata_offset(builder, offset_idx, offset);
}

#[cfg(test)]
mod tests {
    use crate::de::{ArrowDeserializer, FramingIterator};
    use arrow_array::builder::{make_builder, ArrayBuilder};
    use arrow_array::cast::AsArray;
    use arrow_array::types::{GenericBinaryType, Int64Type, TimestampNanosecondType};
    use arrow_array::RecordBatch;
    use arrow_schema::{Schema, TimeUnit};
    use arroyo_rpc::df::ArroyoSchema;
    use arroyo_rpc::formats::{
        BadData, Format, Framing, FramingMethod, JsonFormat, NewlineDelimitedFraming,
        RawBytesFormat,
    };
    use arroyo_types::{to_nanos, SourceError};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::SystemTime;

    #[test]
    fn test_line_framing() {
        let framing = Some(Arc::new(Framing {
            method: FramingMethod::Newline(NewlineDelimitedFraming {
                max_line_length: None,
            }),
        }));

        let result: Vec<_> = FramingIterator::new(framing.clone(), "one block".as_bytes())
            .map(|t| String::from_utf8(t.to_vec()).unwrap())
            .collect();

        assert_eq!(vec!["one block".to_string()], result);

        let result: Vec<_> = FramingIterator::new(
            framing.clone(),
            "one block\ntwo block\nthree block".as_bytes(),
        )
        .map(|t| String::from_utf8(t.to_vec()).unwrap())
        .collect();

        assert_eq!(
            vec![
                "one block".to_string(),
                "two block".to_string(),
                "three block".to_string(),
            ],
            result
        );

        let result: Vec<_> = FramingIterator::new(
            framing.clone(),
            "one block\ntwo block\nthree block\n".as_bytes(),
        )
        .map(|t| String::from_utf8(t.to_vec()).unwrap())
        .collect();

        assert_eq!(
            vec![
                "one block".to_string(),
                "two block".to_string(),
                "three block".to_string(),
            ],
            result
        );
    }

    #[test]
    fn test_max_line_length() {
        let framing = Some(Arc::new(Framing {
            method: FramingMethod::Newline(NewlineDelimitedFraming {
                max_line_length: Some(5),
            }),
        }));

        let result: Vec<_> =
            FramingIterator::new(framing, "one block\ntwo block\nwhole".as_bytes())
                .map(|t| String::from_utf8(t.to_vec()).unwrap())
                .collect();

        assert_eq!(
            vec![
                "one b".to_string(),
                "two b".to_string(),
                "whole".to_string(),
            ],
            result
        );
    }

    fn setup_deserializer(bad_data: BadData) -> (Vec<Box<dyn ArrayBuilder>>, ArrowDeserializer) {
        let schema = Arc::new(Schema::new(vec![
            arrow_schema::Field::new("x", arrow_schema::DataType::Int64, true),
            arrow_schema::Field::new(
                "_timestamp",
                arrow_schema::DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            ),
        ]));

        let arrays: Vec<_> = schema
            .fields
            .iter()
            .map(|f| make_builder(f.data_type(), 16))
            .collect();

        let schema = ArroyoSchema::from_schema_unkeyed(schema).unwrap();

        let deserializer = ArrowDeserializer::new(
            Format::Json(JsonFormat {
                confluent_schema_registry: false,
                schema_id: None,
                include_schema: false,
                debezium: false,
                unstructured: false,
                timestamp_format: Default::default(),
            }),
            schema,
            None,
            bad_data,
        );

        (arrays, deserializer)
    }

    #[tokio::test]
    async fn test_bad_data_drop() {
        let (mut arrays, mut deserializer) = setup_deserializer(BadData::Drop {});

        let now = SystemTime::now();

        assert_eq!(
            deserializer
                .deserialize_slice(
                    &mut arrays[..],
                    json!({ "x": 5 }).to_string().as_bytes(),
                    now,
                    (false, 0, 0, "".to_string())
                )
                .await,
            vec![]
        );
        assert_eq!(
            deserializer
                .deserialize_slice(
                    &mut arrays[..],
                    json!({ "x": "hello" }).to_string().as_bytes(),
                    now,
                    (false, 0, 0, "".to_string())
                )
                .await,
            vec![]
        );

        let batch = deserializer.flush_buffer().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.columns()[0].as_primitive::<Int64Type>().value(0), 5);
        assert_eq!(
            batch.columns()[1]
                .as_primitive::<TimestampNanosecondType>()
                .value(0),
            to_nanos(now) as i64
        );
    }

    #[tokio::test]
    async fn test_bad_data_fail() {
        let (mut arrays, mut deserializer) = setup_deserializer(BadData::Fail {});

        assert_eq!(
            deserializer
                .deserialize_slice(
                    &mut arrays[..],
                    json!({ "x": 5 }).to_string().as_bytes(),
                    SystemTime::now(),
                    (false, 0, 0, "".to_string())
                )
                .await,
            vec![]
        );
        assert_eq!(
            deserializer
                .deserialize_slice(
                    &mut arrays[..],
                    json!({ "x": "hello" }).to_string().as_bytes(),
                    SystemTime::now(),
                    (false, 0, 0, "".to_string())
                )
                .await,
            vec![]
        );

        let err = deserializer.flush_buffer().unwrap().unwrap_err();

        assert!(matches!(err, SourceError::BadData { .. }));
    }

    #[tokio::test]
    async fn test_raw_bytes() {
        let schema = Arc::new(Schema::new(vec![
            arrow_schema::Field::new("value", arrow_schema::DataType::Binary, false),
            arrow_schema::Field::new(
                "_timestamp",
                arrow_schema::DataType::Timestamp(TimeUnit::Nanosecond, None),
                false,
            ),
        ]));

        let mut arrays: Vec<_> = schema
            .fields
            .iter()
            .map(|f| make_builder(f.data_type(), 16))
            .collect();

        let arroyo_schema = ArroyoSchema::from_schema_unkeyed(schema.clone()).unwrap();

        let mut deserializer = ArrowDeserializer::new(
            Format::RawBytes(RawBytesFormat {}),
            arroyo_schema,
            None,
            BadData::Fail {},
        );

        let time = SystemTime::now();
        let result = deserializer
            .deserialize_slice(
                &mut arrays,
                &[0, 1, 2, 3, 4, 5],
                time,
                (false, 0, 0, "".to_string()),
            )
            .await;
        assert!(result.is_empty());

        let arrays: Vec<_> = arrays.into_iter().map(|mut a| a.finish()).collect();
        let batch = RecordBatch::try_new(schema, arrays).unwrap();

        assert_eq!(batch.num_rows(), 1);
        assert_eq!(
            batch.columns()[0]
                .as_bytes::<GenericBinaryType<i32>>()
                .value(0),
            &[0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            batch.columns()[1]
                .as_primitive::<TimestampNanosecondType>()
                .value(0),
            to_nanos(time) as i64
        );
    }
}
