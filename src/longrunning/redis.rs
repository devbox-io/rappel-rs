use std::collections::HashMap;
use std::marker::PhantomData;

use chrono::Utc;
use prost::Message;
use redis::from_redis_value;
use redis::AsyncCommands;
use redis::FromRedisValue;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing_futures::Instrument;
use uuid::Uuid;

use crate::codec::json::JsonCodec;
use crate::codec::Codec;
use crate::codec::Decoder;
use crate::codec::Encoder;
use crate::proto::google::rpc::Status;
use crate::proto::longrunning::Operation;

use super::Broker;
use super::Context;
use super::Performable;
use super::Queue;

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
  #[error("Failed to enqueue the task: {0}")]
  QueueError(#[from] RedisQueueError),
}

#[derive(Clone, Debug)]
pub struct RedisBroker<T: Serialize + DeserializeOwned + Performable> {
  _client: redis::Client,
  queue: RedisQueue<T, JsonCodec<T, T>>,
  _phantom: PhantomData<T>,
}

impl<T: Send + Sync + Serialize + DeserializeOwned + Performable> RedisBroker<T> {
  pub fn new(client: redis::Client, queue_name: &str) -> Self {
    Self {
      _client: client.clone(),
      queue: RedisQueue::new(client, queue_name.to_string(), JsonCodec::new()),
      _phantom: PhantomData,
    }
  }
}

#[async_trait::async_trait]
impl<T: Performable> Broker<T> for RedisBroker<T>
where
  T: Send + Sync + Serialize + DeserializeOwned,
{
  type Error = BrokerError;

  async fn enqueue(&self, task: T, ctx: &Context) -> Result<Operation, Self::Error> {
    let id = self.queue.offer(task, ctx).await?;

    let operation = Operation {
      operation_id: id,
      metadata: HashMap::default(),
      done: false,
      error: None,
      response: HashMap::default(),
      creation_ts: None,
      start_ts: None,
      end_ts: None,
    };

    Ok(operation)
  }

  async fn cancel(&self, _id: &str, _ctx: &Context) -> Result<Operation, Self::Error> {
    todo!()
  }
}

#[derive(Clone, Debug)]
pub struct RedisQueue<T, C: Codec> {
  client: redis::Client,
  queue: String,
  codec: C,
  _phantom: PhantomData<T>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedisMessage<T> {
  pub ack_id: String,
  pub data: T,
}

#[derive(thiserror::Error, Debug)]
pub enum RedisQueueError {
  #[error("Redis command failed: {0}")]
  Redis(#[from] redis::RedisError),

  #[error("Failed at codec")]
  CodecError(#[from] crate::codec::json::Error),

  #[error("Invalid Task Type. Expected {0}, Found {1}")]
  InvalidTaskType(String, String),

  #[error("Internal: {0}")]
  Internal(String),

  #[error("NotFound: {0}")]
  NotFound(String),

  #[error("Unknown")]
  Unknown(#[from] anyhow::Error),
}

impl<T> super::Task<T> for RedisMessage<T> {
  fn ack_id(&self) -> &str {
    &self.ack_id
  }

  fn data(&self) -> &T {
    &self.data
  }
}

impl<T: Performable, C: Codec> RedisQueue<T, C> {
  pub fn new(client: redis::Client, queue: String, codec: C) -> Self {
    Self {
      client,
      queue,
      codec,
      _phantom: PhantomData,
    }
  }

  pub async fn complete<M: Message, E: Into<Status>>(
    &self,
    id: &str,
    r: Result<M, E>,
    _ctx: &Context,
  ) -> Result<(), RedisQueueError> {
    let mut conn = self.client.get_async_connection().await?;
    let mut pipe = redis::pipe();

    let mut pipeline = pipe
      .atomic()
      .hset_multiple(
        format!("operation:{}", id),
        &[
          ("done", "true"),
          ("status", "Terminated"),
          ("end_ts", &Utc::now().timestamp_nanos().to_string()),
        ],
      )
      .ignore();

    pipeline = match r {
      Err(error) => {
        let status: Status = error.into();

        pipeline
          .hset(format!("operation:{}", id), "error", status.encode_to_vec())
          .ignore()
      }
      Ok(output) => pipeline
        .hset(
          format!("operation:{}", id),
          "result",
          output.encode_to_vec(),
        )
        .ignore(),
    };

    let _ = pipeline
      .query_async(&mut conn)
      .instrument(tracing::info_span!("redis-queue-complete"))
      .await?;

    Ok(())
  }
}

#[async_trait::async_trait]
impl<T: Send + Sync + Serialize + DeserializeOwned + Performable> super::Queue
  for RedisQueue<T, JsonCodec<T, T>>
{
  type Item = T;

  type ReceivedItem = RedisMessage<T>;

  type Error = RedisQueueError;

  async fn offer(&self, item: Self::Item, ctx: &Context) -> Result<String, Self::Error> {
    let mut encoder = self.codec.encoder();
    let id = Uuid::new_v4().to_string();
    let publish_ts = Utc::now().timestamp_nanos();

    let mut task = Vec::default();
    let _ = encoder.encode(&item, &mut task);
    let task = String::from_utf8_lossy(&task);

    let mut conn = self.client.get_async_connection().await?;

    let _ = redis::pipe()
      .atomic()
      .lpush(format!("queue:{}", self.queue), id.clone())
      .ignore()
      .hset_multiple(
        format!("operation:{}", id),
        &[
          ("status", "New"),
          ("operation_id", &id),
          ("queue", &self.queue),
          ("publish_ts", &publish_ts.to_string()),
          ("task", &task),
          ("user_id", ctx.user_id()),
          ("task_type", Self::Item::type_name()),
        ],
      )
      .ignore()
      .query_async(&mut conn)
      .instrument(tracing::info_span!("redis-queue-offer", operation_id=%id))
      .await?;

    Ok(id)
  }

  async fn pull(&self, ctx: &Context) -> Result<Option<Self::ReceivedItem>, Self::Error> {
    let mut conn = self.client.get_async_connection().await?;

    let maybe_id: Option<String> = redis::cmd("LMOVE")
      .arg(format!("queue:{}", self.queue))
      .arg(format!("queue:ack:{}", self.queue))
      .arg("RIGHT")
      .arg("LEFT")
      .query_async(&mut conn)
      .instrument(tracing::info_span!("redis-queue-pull-lmove"))
      .await?;

    let op_id = match maybe_id {
      None => return Ok(None),
      Some(id) => id,
    };

    let (op,): (HashMap<String, String>,) = redis::pipe()
      .atomic()
      .hset_multiple(
        format!("operation:{}", op_id),
        &[
          ("dequeue_system_id", ctx.system_id()),
          ("dequeue_ts", &Utc::now().timestamp_nanos().to_string()),
          ("dequeue_user_id", ctx.user_id()),
        ],
      )
      .ignore()
      .hgetall(format!("operation:{}", op_id))
      .query_async(&mut conn)
      .instrument(tracing::info_span!("redis-queue-pull-hget"))
      .await?;

    if op["task_type"] != Self::Item::type_name() {
      tracing::error!(message = "Invalid task type encountered in the queue", task_type = %op["task_type"]);
      return Err(Self::Error::InvalidTaskType(
        std::any::type_name::<Self::Item>().to_string(),
        op["task_type"].to_string(),
      ));
    }

    let mut decoder = self.codec.decoder();
    let task = op["task"].clone();
    let mut buf = task.into_bytes();
    let task: Option<Self::Item> = decoder.decode(&mut buf)?;

    match task {
      Some(t) => Ok(Some(RedisMessage {
        ack_id: op_id,
        data: t,
      })),
      None => Err(Self::Error::Internal("Failed to decode task".to_string())),
    }
  }

  async fn ack(&self, ack_id: &str, ctx: &Context) -> Result<(), Self::Error> {
    let mut conn = self.client.get_async_connection().await?;

    let maybe_queue: Option<String> = conn
      .hget(format!("operation:{}", ack_id), "queue")
      .instrument(tracing::info_span!("redis-queue-ack-hget"))
      .await?;

    let queue = match maybe_queue {
      None => {
        tracing::debug!(message = "Cannot find queue name", %ack_id);
        return Err(Self::Error::NotFound(format!(
          "Missing operation queue info for ack_id = {}",
          ack_id
        )));
      }
      Some(q) => q,
    };

    let _ = redis::pipe()
      .atomic()
      .hset_multiple(
        format!("operation:{}", ack_id),
        &[
          ("ack_system_id", ctx.system_id()),
          ("ack_ts", &Utc::now().timestamp_nanos().to_string()),
          ("ack_user_id", ctx.user_id()),
        ],
      )
      .ignore()
      .lrem(format!("queue:{}", queue), -1, queue)
      .ignore()
      .query_async(&mut conn)
      .instrument(tracing::info_span!("redis-queue-ack-lrem"))
      .await?;

    tracing::debug!(message = "Acknowledged message", %ack_id);
    Ok(())
  }
}

impl FromRedisValue for Operation {
  fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
    let mut map: HashMap<String, String> = from_redis_value(v)?;

    let op = Self {
      operation_id: map.remove("operation_id").unwrap_or(String::default()),
      metadata: HashMap::from([
        (
          "task_type".to_string(),
          map.remove("task_type").unwrap_or(String::default()),
        ),
        (
          "task".to_string(),
          map.remove("task").unwrap_or(String::default()),
        ),
        (
          "user_id".to_string(),
          map.remove("user_id").unwrap_or(String::default()),
        ),
        (
          "queue".to_string(),
          map.remove("queue").unwrap_or(String::default()),
        ),
        (
          "status".to_string(),
          map.remove("status").unwrap_or(String::default()),
        ),
      ]),
      done: map.remove("done").map(|v| v == "true").unwrap_or(false),
      error: None,
      response: HashMap::default(),
      creation_ts: map.remove("publish_ts").map(|v| {
        let ts = v.parse::<i64>().expect("Failed to parse timestamp");
        crate::proto::google::protobuf::Timestamp {
          seconds: ts / 1000_000_000,
          nanos: (ts % 1000_000_000) as i32,
        }
      }),
      start_ts: map.remove("dequeue_ts").map(|v| {
        let ts = v.parse::<i64>().expect("Failed to parse timestamp");
        crate::proto::google::protobuf::Timestamp {
          seconds: ts / 1000_000_000,
          nanos: (ts % 1000_000_000) as i32,
        }
      }),
      end_ts: map.remove("end_ts").map(|v| {
        let ts = v.parse::<i64>().expect("Failed to parse timestamp");
        crate::proto::google::protobuf::Timestamp {
          seconds: ts / 1000_000_000,
          nanos: (ts % 1000_000_000) as i32,
        }
      }),
    };

    Ok(op)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use chrono::Utc;
  use redis::AsyncCommands;
  use serde::Deserialize;

  use crate::{longrunning::Queue, proto::google::protobuf::Empty};

  use super::*;

  #[derive(Serialize, Deserialize, Clone)]
  struct Task {
    item: i32,
  }

  #[async_trait::async_trait]
  impl Performable for Task {
    type Error = std::io::Error;
    type Context = ();
    type Output = Empty;

    fn type_name() -> &'static str {
      "longrunning::redis::tests::Task"
    }

    async fn perform(&self, _: Self::Context) -> Result<Self::Output, Self::Error> {
      Ok(Empty::default())
    }
  }

  #[tokio::test]
  async fn offer_should_add_item_to_queue() {
    let ctx = Context::new(Uuid::new_v4().to_string(), String::from("1234"));
    let queue = Uuid::new_v4().to_string();
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let q: RedisQueue<Task, JsonCodec<Task, Task>> =
      RedisQueue::new(client.clone(), queue.clone(), JsonCodec::new());
    let task = Task { item: 10 };

    let id = q.offer(task, &ctx).await.unwrap();

    let mut conn = client.get_async_connection().await.unwrap();
    let result: Vec<String> = conn
      .lrange(format!("queue:{}", queue), 0, -1)
      .await
      .unwrap();
    assert_eq!(vec![id], result);
  }

  #[tokio::test]
  async fn offer_should_set_metadata_while_adding_item_to_queue() {
    let queue = Uuid::new_v4().to_string();
    let ctx = Context::new(Uuid::new_v4().to_string(), String::from("1234"));
    let ts = Utc::now().timestamp_nanos();
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let q: RedisQueue<Task, JsonCodec<Task, Task>> =
      RedisQueue::new(client.clone(), queue.clone(), JsonCodec::new());
    let task = Task { item: 10 };

    let id = q.offer(task, &ctx).await.unwrap();

    let mut conn = client.get_async_connection().await.unwrap();

    let result: HashMap<String, String> = conn.hgetall(format!("operation:{}", id)).await.unwrap();

    assert_eq!(result["status"], "New");
    assert!(result["publish_ts"].parse::<i64>().unwrap() >= ts);
    assert_eq!(result["task_type"], "longrunning::redis::tests::Task");
    assert_eq!(result["task"], "{\"item\":10}");
    assert_eq!(result["queue"], queue);
    assert_eq!(result["user_id"], ctx.user_id());
  }

  #[tokio::test]
  async fn should_enqueue_task_to_broker() {
    let ctx = Context::new(Uuid::new_v4().to_string(), String::from("1234"));
    let queue = Uuid::new_v4().to_string();
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let q: RedisBroker<Task> = RedisBroker::new(client.clone(), &queue);
    let task = Task { item: 10 };

    let operation = q.enqueue(task, &ctx).await.unwrap();

    let mut conn = client.get_async_connection().await.unwrap();
    let result: Vec<String> = conn
      .lrange(format!("queue:{}", queue), 0, -1)
      .await
      .unwrap();
    assert_eq!(vec![operation.operation_id], result);
  }
}
