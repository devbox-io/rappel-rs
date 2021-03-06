pub use crate::proto::google::protobuf::Timestamp;
use std::ops::Deref;

#[derive(Clone, Debug)]
pub struct ProstTimestamp(pub Timestamp);

impl ProstTimestamp {
  pub fn into_inner(self) -> Timestamp {
    self.0
  }
}

impl Deref for ProstTimestamp {
  type Target = Timestamp;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl From<chrono::DateTime<chrono::Utc>> for ProstTimestamp {
  fn from(timestamp: chrono::DateTime<chrono::Utc>) -> Self {
    let seconds = timestamp.timestamp();
    let nanos = timestamp.timestamp_subsec_nanos() as i32;
    Self(Timestamp { nanos, seconds })
  }
}

impl From<()> for super::google::protobuf::Empty {
  fn from(_: ()) -> Self {
    Self::default()
  }
}
