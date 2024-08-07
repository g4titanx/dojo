pub mod error;
pub mod storage;
pub mod subscription;

use std::cell::OnceCell;
use std::collections::HashSet;
use std::sync::Arc;

use dojo_types::packing::unpack;
use dojo_types::schema::Ty;
use dojo_types::WorldMetadata;
use dojo_world::contracts::{naming, WorldContractReader};
use futures::lock::Mutex;
use parking_lot::{RwLock, RwLockReadGuard};
use starknet::core::types::Felt;
use starknet::providers::jsonrpc::HttpTransport;
use starknet::providers::JsonRpcClient;
use tokio::sync::RwLock as AsyncRwLock;
use torii_grpc::client::{EntityUpdateStreaming, EventUpdateStreaming, ModelDiffsStreaming};
use torii_grpc::proto::world::{RetrieveEntitiesResponse, RetrieveEventsResponse};
use torii_grpc::types::schema::Entity;
use torii_grpc::types::{EntityKeysClause, Event, EventQuery, KeysClause, ModelKeysClause, Query};
use torii_relay::client::EventLoop;
use torii_relay::types::Message;

use crate::client::error::{Error, ParseError};
use crate::client::storage::ModelStorage;
use crate::client::subscription::{
    SubscribedModels, SubscriptionClientHandle, SubscriptionService,
};

// TODO: remove reliance on RPC
#[allow(unused)]
#[derive(Debug)]
pub struct Client {
    /// Metadata of the World that the client is connected to.
    metadata: Arc<RwLock<WorldMetadata>>,
    /// The grpc client.
    inner: AsyncRwLock<torii_grpc::client::WorldClient>,
    /// Relay client.
    relay_client: torii_relay::client::RelayClient,
    /// Model storage
    storage: Arc<ModelStorage>,
    /// Models the client are subscribed to.
    subscribed_models: Arc<SubscribedModels>,
    /// The subscription client handle.
    sub_client_handle: OnceCell<SubscriptionClientHandle>,
    /// World contract reader.
    world_reader: WorldContractReader<JsonRpcClient<HttpTransport>>,
}

impl Client {
    /// Returns a initialized [Client].
    pub async fn new(
        torii_url: String,
        rpc_url: String,
        relay_url: String,
        world: Felt,
    ) -> Result<Self, Error> {
        let mut grpc_client = torii_grpc::client::WorldClient::new(torii_url, world).await?;

        let relay_client = torii_relay::client::RelayClient::new(relay_url)?;

        let metadata = grpc_client.metadata().await?;

        let shared_metadata: Arc<_> = RwLock::new(metadata).into();
        let client_storage: Arc<_> = ModelStorage::new(shared_metadata.clone()).into();
        let subbed_models: Arc<_> = SubscribedModels::new(shared_metadata.clone()).into();

        // initialize the entities to be synced with the latest values
        let rpc_url = url::Url::parse(&rpc_url).map_err(ParseError::Url)?;
        let provider = JsonRpcClient::new(HttpTransport::new(rpc_url));
        let world_reader = WorldContractReader::new(world, provider);

        Ok(Self {
            world_reader,
            storage: client_storage,
            metadata: shared_metadata,
            sub_client_handle: OnceCell::new(),
            inner: AsyncRwLock::new(grpc_client),
            relay_client,
            subscribed_models: subbed_models,
        })
    }

    /// Starts the relay client event loop.
    /// This is a blocking call. Spawn this on a separate task.
    pub fn relay_runner(&self) -> Arc<Mutex<EventLoop>> {
        self.relay_client.event_loop.clone()
    }

    /// Publishes a message to a topic.
    /// Returns the message id.
    pub async fn publish_message(&self, message: Message) -> Result<Vec<u8>, Error> {
        self.relay_client
            .command_sender
            .publish(message)
            .await
            .map_err(Error::RelayClient)
            .map(|m| m.0)
    }

    /// Returns a read lock on the World metadata that the client is connected to.
    pub fn metadata(&self) -> RwLockReadGuard<'_, WorldMetadata> {
        self.metadata.read()
    }

    pub fn subscribed_models(&self) -> RwLockReadGuard<'_, HashSet<ModelKeysClause>> {
        self.subscribed_models.models_keys.read()
    }

    /// Retrieves entities matching query parameter.
    ///
    /// The query param includes an optional clause for filtering. Without clause, it fetches ALL
    /// entities, this is less efficient as it requires an additional query for each entity's
    /// model data. Specifying a clause can optimize the query by limiting the retrieval to specific
    /// type of entites matching keys and/or models.
    pub async fn entities(&self, query: Query) -> Result<Vec<Entity>, Error> {
        let mut grpc_client = self.inner.write().await;
        let RetrieveEntitiesResponse { entities, total_count: _ } =
            grpc_client.retrieve_entities(query).await?;
        Ok(entities.into_iter().map(TryInto::try_into).collect::<Result<Vec<Entity>, _>>()?)
    }

    /// Similary to entities, this function retrieves event messages matching the query parameter.
    pub async fn event_messages(&self, query: Query) -> Result<Vec<Entity>, Error> {
        let mut grpc_client = self.inner.write().await;
        let RetrieveEntitiesResponse { entities, total_count: _ } =
            grpc_client.retrieve_event_messages(query).await?;
        Ok(entities.into_iter().map(TryInto::try_into).collect::<Result<Vec<Entity>, _>>()?)
    }

    /// Retrieve raw starknet events matching the keys provided.
    /// If the keys are empty, it will return all events.
    pub async fn starknet_events(&self, query: EventQuery) -> Result<Vec<Event>, Error> {
        let mut grpc_client = self.inner.write().await;
        let RetrieveEventsResponse { events } = grpc_client.retrieve_events(query).await?;
        Ok(events.into_iter().map(Event::from).collect::<Vec<Event>>())
    }

    /// A direct stream to grpc subscribe entities
    pub async fn on_entity_updated(
        &self,
        clauses: Vec<EntityKeysClause>,
    ) -> Result<EntityUpdateStreaming, Error> {
        let mut grpc_client = self.inner.write().await;
        let stream = grpc_client.subscribe_entities(clauses).await?;
        Ok(stream)
    }

    /// Update the entities subscription
    pub async fn update_entity_subscription(
        &self,
        subscription_id: u64,
        clauses: Vec<EntityKeysClause>,
    ) -> Result<(), Error> {
        let mut grpc_client = self.inner.write().await;
        grpc_client.update_entities_subscription(subscription_id, clauses).await?;
        Ok(())
    }

    /// A direct stream to grpc subscribe event messages
    pub async fn on_event_message_updated(
        &self,
        clauses: Vec<EntityKeysClause>,
    ) -> Result<EntityUpdateStreaming, Error> {
        let mut grpc_client = self.inner.write().await;
        let stream = grpc_client.subscribe_event_messages(clauses).await?;
        Ok(stream)
    }

    /// Update the event messages subscription
    pub async fn update_event_message_subscription(
        &self,
        subscription_id: u64,
        clauses: Vec<EntityKeysClause>,
    ) -> Result<(), Error> {
        let mut grpc_client = self.inner.write().await;
        grpc_client.update_event_messages_subscription(subscription_id, clauses).await?;
        Ok(())
    }

    pub async fn on_starknet_event(
        &self,
        keys: Option<KeysClause>,
    ) -> Result<EventUpdateStreaming, Error> {
        let mut grpc_client = self.inner.write().await;
        let stream = grpc_client.subscribe_events(keys).await?;
        Ok(stream)
    }

    /// Returns the value of a model.
    ///
    /// This function will only return `None`, if `model` doesn't exist. If there is no model with
    /// the specified `keys`, it will return a [`Ty`] with the default values.
    ///
    /// If the requested model is not among the synced models, it will attempt to fetch it from
    /// the RPC.
    pub async fn model(&self, keys: &ModelKeysClause) -> Result<Option<Ty>, Error> {
        let (namespace, model) = keys.model.split_once('-').unwrap();
        let model_selector = naming::compute_selector_from_names(namespace, model);
        let Some(mut schema) =
            self.metadata.read().model(&model_selector).map(|m| m.schema.clone())
        else {
            return Ok(None);
        };

        if !self.subscribed_models.is_synced(keys) {
            let model = self.world_reader.model_reader(namespace, model).await?;
            return Ok(Some(model.entity(&keys.keys).await?));
        }

        let Ok(Some(raw_values)) = self.storage.get_model_storage(model_selector, &keys.keys)
        else {
            return Ok(Some(schema));
        };

        let layout = self
            .metadata
            .read()
            .model(&model_selector)
            .map(|m| m.layout.clone())
            .expect("qed; layout should exist");

        let unpacked = unpack(raw_values, layout).unwrap();
        let mut keys_and_unpacked = [keys.keys.to_vec(), unpacked].concat();

        schema.deserialize(&mut keys_and_unpacked).unwrap();

        Ok(Some(schema))
    }

    /// Initiate the model subscriptions and returns a [SubscriptionService] which when await'ed
    /// will execute the subscription service and starts the syncing process.
    pub async fn start_subscription(&self) -> Result<SubscriptionService, Error> {
        let models_keys: Vec<ModelKeysClause> =
            self.subscribed_models.models_keys.read().clone().into_iter().collect();
        let sub_res_stream = self.initiate_subscription(models_keys).await?;

        let (service, handle) = SubscriptionService::new(
            Arc::clone(&self.storage),
            Arc::clone(&self.metadata),
            Arc::clone(&self.subscribed_models),
            sub_res_stream,
        );

        self.sub_client_handle.set(handle).unwrap();
        Ok(service)
    }

    /// Adds entities to the list of entities to be synced.
    ///
    /// NOTE: This will establish a new subscription stream with the server.
    pub async fn add_models_to_sync(&self, models_keys: Vec<ModelKeysClause>) -> Result<(), Error> {
        for keys in &models_keys {
            let (namespace, model) = keys.model.split_once('-').unwrap();
            self.initiate_model(namespace, model, keys.keys.clone()).await?;
        }

        self.subscribed_models.add_models(models_keys)?;

        let updated_models =
            self.subscribed_models.models_keys.read().clone().into_iter().collect();
        let sub_res_stream = self.initiate_subscription(updated_models).await?;

        match self.sub_client_handle.get() {
            Some(handle) => handle.update_subscription_stream(sub_res_stream),
            None => return Err(Error::SubscriptionUninitialized),
        }
        Ok(())
    }

    /// Removes models from the list of models to be synced.
    ///
    /// NOTE: This will establish a new subscription stream with the server.
    pub async fn remove_models_to_sync(
        &self,
        models_keys: Vec<ModelKeysClause>,
    ) -> Result<(), Error> {
        self.subscribed_models.remove_models(models_keys)?;

        let updated_entities =
            self.subscribed_models.models_keys.read().clone().into_iter().collect();
        let sub_res_stream = self.initiate_subscription(updated_entities).await?;

        match self.sub_client_handle.get() {
            Some(handle) => handle.update_subscription_stream(sub_res_stream),
            None => return Err(Error::SubscriptionUninitialized),
        }
        Ok(())
    }

    pub fn storage(&self) -> Arc<ModelStorage> {
        Arc::clone(&self.storage)
    }

    async fn initiate_subscription(
        &self,
        keys: Vec<ModelKeysClause>,
    ) -> Result<ModelDiffsStreaming, Error> {
        let mut grpc_client = self.inner.write().await;
        let stream = grpc_client.subscribe_model_diffs(keys).await?;
        Ok(stream)
    }

    async fn initiate_model(
        &self,
        namespace: &str,
        model: &str,
        keys: Vec<Felt>,
    ) -> Result<(), Error> {
        let model_reader = self.world_reader.model_reader(namespace, model).await?;
        let values = model_reader.entity_storage(&keys).await?;
        self.storage.set_model_storage(
            naming::compute_selector_from_names(namespace, model),
            keys,
            values,
        )?;
        Ok(())
    }
}
