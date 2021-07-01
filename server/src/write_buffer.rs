use async_trait::async_trait;
use data_types::database_rules::{DatabaseRules, WriteBufferConnection};
use entry::{Entry, Sequence, SequencedEntry};
use futures::{stream::BoxStream, StreamExt};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    error::KafkaError,
    producer::{FutureProducer, FutureRecord},
    ClientConfig, Message, Offset, TopicPartitionList,
};
use std::{
    convert::{TryFrom, TryInto},
    sync::Arc,
};

pub type WriteBufferError = Box<dyn std::error::Error + Sync + Send>;

#[derive(Debug)]
pub enum WriteBufferConfig {
    Writing(Arc<dyn WriteBufferWriting>),
    Reading(Arc<dyn WriteBufferReading>),
}

impl WriteBufferConfig {
    pub fn new(rules: &DatabaseRules) -> Result<Option<Self>, WriteBufferError> {
        let name = rules.db_name();

        // Right now, the Kafka producer and consumers ar the only production implementations of the
        // `WriteBufferWriting` and `WriteBufferReading` traits. If/when there are other kinds of
        // write buffers, additional configuration will be needed to determine what kind of write
        // buffer to use here.
        match rules.write_buffer_connection.as_ref() {
            Some(WriteBufferConnection::Writing(conn)) => {
                let kafka_buffer = KafkaBufferProducer::new(conn, name)?;

                Ok(Some(Self::Writing(Arc::new(kafka_buffer) as _)))
            }
            Some(WriteBufferConnection::Reading(conn)) => {
                let kafka_buffer = KafkaBufferConsumer::new(conn, name)?;

                Ok(Some(Self::Reading(Arc::new(kafka_buffer) as _)))
            }
            None => Ok(None),
        }
    }
}

/// Writing to a Write Buffer takes an `Entry` and returns `Sequence` data that facilitates reading
/// entries from the Write Buffer at a later time.
#[async_trait]
pub trait WriteBufferWriting: Sync + Send + std::fmt::Debug + 'static {
    /// Send an `Entry` to the write buffer and return information that can be used to restore
    /// entries at a later time.
    async fn store_entry(&self, entry: &Entry) -> Result<Sequence, WriteBufferError>;
}

/// Produce a stream of `SequencedEntry` that a `Db` can add to the mutable buffer by using
/// `Db::stream_in_sequenced_entries`.
pub trait WriteBufferReading: Sync + Send + std::fmt::Debug + 'static {
    fn stream<'life0, 'async_trait>(
        &'life0 self,
    ) -> BoxStream<'async_trait, Result<SequencedEntry, WriteBufferError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait;
}

pub struct KafkaBufferProducer {
    conn: String,
    database_name: String,
    producer: FutureProducer,
}

// Needed because rdkafka's FutureProducer doesn't impl Debug
impl std::fmt::Debug for KafkaBufferProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaBufferProducer")
            .field("conn", &self.conn)
            .field("database_name", &self.database_name)
            .finish()
    }
}

#[async_trait]
impl WriteBufferWriting for KafkaBufferProducer {
    /// Send an `Entry` to Kafka and return the partition ID as the sequencer ID and the offset
    /// as the sequence number.
    async fn store_entry(&self, entry: &Entry) -> Result<Sequence, WriteBufferError> {
        // This type annotation is necessary because `FutureRecord` is generic over key type, but
        // key is optional and we're not setting a key. `String` is arbitrary.
        let record: FutureRecord<'_, String, _> =
            FutureRecord::to(&self.database_name).payload(entry.data());

        // Can't use `?` here because `send_result` returns `Err((E: Error, original_msg))` so we
        // have to extract the actual error out with a `match`.
        let (partition, offset) = match self.producer.send_result(record) {
            // Same error structure on the result of the future, need to `match`
            Ok(delivery_future) => match delivery_future.await? {
                Ok((partition, offset)) => (partition, offset),
                Err((e, _returned_record)) => return Err(Box::new(e)),
            },
            Err((e, _returned_record)) => return Err(Box::new(e)),
        };

        Ok(Sequence {
            id: partition.try_into()?,
            number: offset.try_into()?,
        })
    }
}

impl KafkaBufferProducer {
    pub fn new(
        conn: impl Into<String>,
        database_name: impl Into<String>,
    ) -> Result<Self, KafkaError> {
        let conn = conn.into();
        let database_name = database_name.into();

        let mut cfg = ClientConfig::new();
        cfg.set("bootstrap.servers", &conn);
        cfg.set("message.timeout.ms", "5000");

        let producer: FutureProducer = cfg.create()?;

        Ok(Self {
            conn,
            database_name,
            producer,
        })
    }
}

pub struct KafkaBufferConsumer {
    conn: String,
    database_name: String,
    consumer: StreamConsumer,
}

// Needed because rdkafka's StreamConsumer doesn't impl Debug
impl std::fmt::Debug for KafkaBufferConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KafkaBufferConsumer")
            .field("conn", &self.conn)
            .field("database_name", &self.database_name)
            .finish()
    }
}

impl WriteBufferReading for KafkaBufferConsumer {
    fn stream<'life0, 'async_trait>(
        &'life0 self,
    ) -> BoxStream<'async_trait, Result<SequencedEntry, WriteBufferError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.consumer
            .stream()
            .map(|message| {
                let message = message?;
                let entry = Entry::try_from(message.payload().unwrap().to_vec())?;
                let sequence = Sequence {
                    id: message.partition().try_into()?,
                    number: message.offset().try_into()?,
                };

                Ok(SequencedEntry::new_from_sequence(sequence, entry)?)
            })
            .boxed()
    }
}

impl KafkaBufferConsumer {
    pub fn new(
        conn: impl Into<String>,
        database_name: impl Into<String>,
    ) -> Result<Self, KafkaError> {
        let conn = conn.into();
        let database_name = database_name.into();

        let mut cfg = ClientConfig::new();
        cfg.set("bootstrap.servers", &conn);
        cfg.set("session.timeout.ms", "6000");
        cfg.set("enable.auto.commit", "false");
        cfg.set("group.id", "placeholder");

        let consumer: StreamConsumer = cfg.create()?;
        let mut topics = TopicPartitionList::new();
        topics.add_partition(&database_name, 0);
        topics
            .set_partition_offset(&database_name, 0, Offset::Beginning)
            .unwrap();
        consumer.assign(&topics)?;

        Ok(Self {
            conn,
            database_name,
            consumer,
        })
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use futures::stream::{self, StreamExt, TryStreamExt};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    pub struct MockBufferForWriting {
        pub entries: Arc<Mutex<Vec<Entry>>>,
    }

    #[async_trait]
    impl WriteBufferWriting for MockBufferForWriting {
        async fn store_entry(&self, entry: &Entry) -> Result<Sequence, WriteBufferError> {
            let mut entries = self.entries.lock().unwrap();
            let offset = entries.len() as u64;
            entries.push(entry.clone());

            Ok(Sequence {
                id: 0,
                number: offset,
            })
        }
    }

    type MoveableEntries = Arc<Mutex<Vec<Result<Entry, WriteBufferError>>>>;
    pub struct MockBufferForReading {
        entries: MoveableEntries,
    }

    impl std::fmt::Debug for MockBufferForReading {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockBufferForReading").finish()
        }
    }

    impl MockBufferForReading {
        pub fn new(entries: Vec<Result<Entry, WriteBufferError>>) -> Self {
            Self {
                entries: Arc::new(Mutex::new(entries)),
            }
        }
    }

    impl WriteBufferReading for MockBufferForReading {
        fn stream<'life0, 'async_trait>(
            &'life0 self,
        ) -> BoxStream<'async_trait, Result<SequencedEntry, WriteBufferError>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            // move the entries out of `self` to move them into the stream
            let entries: Vec<_> = self.entries.lock().unwrap().drain(..).collect();

            stream::iter(entries.into_iter())
                .chain(stream::pending())
                .map_ok(SequencedEntry::new_unsequenced)
                .boxed()
        }
    }
}
