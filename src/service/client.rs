use tonic::transport::Channel;

use super::config::ServiceConf;

#[derive(Clone, Debug)]
pub struct ShardedClient<T: Clone> {
  name: String,
  clients: Vec<T>,
}

impl <T: Clone> ShardedClient<T> {
  pub(crate) async fn try_new<F: Fn(Channel) -> T>(config: ServiceConf, builder: F) -> Result<Self, super::Error> {
    let name = config.name;
    let mut clients = Vec::default();

    for instance in config.instances {
      let address = instance.address.clone();
      let channel = tonic::transport::Channel::from_shared(address)?.connect().await?;
      clients.push(builder(channel));
    }

    let client = Self { name, clients };

    Ok(client)
  }

  pub fn borrow(&self, key: &str) -> Result<&T, super::Error> {
    self.clients.get(0).ok_or(super::Error::MissingClient(key.to_string()))
  }

  pub fn borrow_mut(&mut self, key: &str) -> Result<&mut T, super::Error> {
    self.clients.get_mut(0).ok_or(super::Error::MissingClient(key.to_string()))
  }
}
