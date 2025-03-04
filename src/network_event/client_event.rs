use std::fmt::Debug;

use bevy::{ecs::event::Event, prelude::*};
use bevy_renet::{
    renet::{RenetClient, RenetServer, SendType},
    transport::client_connected,
};
use bincode::{DefaultOptions, Options};
use serde::{
    de::{DeserializeOwned, DeserializeSeed},
    Serialize,
};

use super::{BuildEventDeserializer, BuildEventSerializer, EventChannel};
use crate::{
    client::{ClientSet, NetworkEntityMap},
    network_event::EventMapper,
    replicon_core::{replication_rules::MapNetworkEntities, NetworkChannels},
    server::{has_authority, ServerSet, SERVER_ID},
};

/// An extension trait for [`App`] for creating client events.
pub trait ClientEventAppExt {
    /// Registers [`FromClient<T>`] event that will be emitted on server after sending `T` event on client.
    fn add_client_event<T: Event + Serialize + DeserializeOwned + Debug>(
        &mut self,
        policy: impl Into<SendType>,
    ) -> &mut Self;

    /// Same as [`Self::add_client_event`], but additionally maps client entities to server before sending.
    fn add_mapped_client_event<
        T: Event + Serialize + DeserializeOwned + Debug + MapNetworkEntities,
    >(
        &mut self,
        policy: impl Into<SendType>,
    ) -> &mut Self;

    /// Same as [`Self::add_client_event`], but the event will be serialized/deserialized using `S`/`D`
    /// with access to [`AppTypeRegistry`].
    ///
    /// Needed to send events that contain things like `Box<dyn Reflect>`.
    fn add_client_reflect_event<T, S, D>(&mut self, policy: impl Into<SendType>) -> &mut Self
    where
        T: Event + Debug,
        S: BuildEventSerializer<T> + 'static,
        D: BuildEventDeserializer + 'static,
        for<'a> S::EventSerializer<'a>: Serialize,
        for<'a, 'de> D::EventDeserializer<'a>: DeserializeSeed<'de, Value = T>;

    /// Same as [`Self::add_client_reflect_event`], but additionally maps client entities to server before sending.
    fn add_mapped_client_reflect_event<T, S, D>(
        &mut self,
        policy: impl Into<SendType>,
    ) -> &mut Self
    where
        T: Event + Debug + MapNetworkEntities,
        S: BuildEventSerializer<T> + 'static,
        D: BuildEventDeserializer + 'static,
        for<'a> S::EventSerializer<'a>: Serialize,
        for<'a, 'de> D::EventDeserializer<'a>: DeserializeSeed<'de, Value = T>;

    /// Same as [`Self::add_client_event`], but uses specified sending and receiving systems.
    fn add_client_event_with<T: Event + Debug, Marker1, Marker2>(
        &mut self,
        policy: impl Into<SendType>,
        sending_system: impl IntoSystemConfigs<Marker1>,
        receiving_system: impl IntoSystemConfigs<Marker2>,
    ) -> &mut Self;
}

impl ClientEventAppExt for App {
    fn add_client_event<T: Event + Serialize + DeserializeOwned + Debug>(
        &mut self,
        policy: impl Into<SendType>,
    ) -> &mut Self {
        self.add_client_event_with::<T, _, _>(policy, sending_system::<T>, receiving_system::<T>)
    }

    fn add_mapped_client_event<
        T: Event + Serialize + DeserializeOwned + Debug + MapNetworkEntities,
    >(
        &mut self,
        policy: impl Into<SendType>,
    ) -> &mut Self {
        self.add_client_event_with::<T, _, _>(
            policy,
            mapping_and_sending_system::<T>,
            receiving_system::<T>,
        )
    }

    fn add_client_reflect_event<T, S, D>(&mut self, policy: impl Into<SendType>) -> &mut Self
    where
        T: Event + Debug,
        S: BuildEventSerializer<T> + 'static,
        D: BuildEventDeserializer + 'static,
        for<'a> S::EventSerializer<'a>: Serialize,
        for<'a, 'de> D::EventDeserializer<'a>: DeserializeSeed<'de, Value = T>,
    {
        self.add_client_event_with::<T, _, _>(
            policy,
            sending_reflect_system::<T, S>,
            receiving_reflect_system::<T, D>,
        )
    }

    fn add_mapped_client_reflect_event<T, S, D>(&mut self, policy: impl Into<SendType>) -> &mut Self
    where
        T: Event + Debug + MapNetworkEntities,
        S: BuildEventSerializer<T> + 'static,
        D: BuildEventDeserializer + 'static,
        for<'a> S::EventSerializer<'a>: Serialize,
        for<'a, 'de> D::EventDeserializer<'a>: DeserializeSeed<'de, Value = T>,
    {
        self.add_client_event_with::<T, _, _>(
            policy,
            mapping_and_sending_reflect_system::<T, S>,
            receiving_reflect_system::<T, D>,
        )
    }

    fn add_client_event_with<T: Event + Debug, Marker1, Marker2>(
        &mut self,
        policy: impl Into<SendType>,
        sending_system: impl IntoSystemConfigs<Marker1>,
        receiving_system: impl IntoSystemConfigs<Marker2>,
    ) -> &mut Self {
        let channel_id = self
            .world
            .resource_mut::<NetworkChannels>()
            .create_client_channel(policy.into());

        self.add_event::<T>()
            .init_resource::<Events<FromClient<T>>>()
            .insert_resource(EventChannel::<T>::new(channel_id))
            .add_systems(
                PreUpdate,
                receiving_system
                    .in_set(ServerSet::Receive)
                    .run_if(resource_exists::<RenetServer>()),
            )
            .add_systems(
                PostUpdate,
                (
                    sending_system.run_if(client_connected()),
                    local_resending_system::<T>.run_if(has_authority()),
                )
                    .chain()
                    .in_set(ClientSet::Send),
            );

        self
    }
}

fn receiving_system<T: Event + DeserializeOwned + Debug>(
    mut client_events: EventWriter<FromClient<T>>,
    mut server: ResMut<RenetServer>,
    channel: Res<EventChannel<T>>,
) {
    for client_id in server.clients_id() {
        while let Some(message) = server.receive_message(client_id, channel.id) {
            match DefaultOptions::new().deserialize(&message) {
                Ok(event) => {
                    debug!("received event {event:?} from client {client_id}");
                    client_events.send(FromClient { client_id, event });
                }
                Err(e) => error!("unable to deserialize event from client {client_id}: {e}"),
            }
        }
    }
}

fn receiving_reflect_system<T, D>(
    mut client_events: EventWriter<FromClient<T>>,
    mut server: ResMut<RenetServer>,
    channel: Res<EventChannel<T>>,
    registry: Res<AppTypeRegistry>,
) where
    T: Event + Debug,
    D: BuildEventDeserializer,
    for<'a, 'de> D::EventDeserializer<'a>: DeserializeSeed<'de, Value = T>,
{
    let registry = registry.read();
    for client_id in server.clients_id() {
        while let Some(message) = server.receive_message(client_id, channel.id) {
            let mut deserializer =
                bincode::Deserializer::from_slice(&message, DefaultOptions::new());
            match D::new(&registry).deserialize(&mut deserializer) {
                Ok(event) => {
                    debug!("received reflect event {event:?} from client {client_id}");
                    client_events.send(FromClient { client_id, event });
                }
                Err(e) => {
                    error!("unable to deserialize reflect event from client {client_id}: {e}")
                }
            }
        }
    }
}

fn sending_system<T: Event + Serialize + Debug>(
    mut events: EventReader<T>,
    mut client: ResMut<RenetClient>,
    channel: Res<EventChannel<T>>,
) {
    for event in &mut events {
        let message = DefaultOptions::new()
            .serialize(&event)
            .expect("client event should be serializable");
        client.send_message(channel.id, message);
        debug!("sent client event {event:?}");
    }
}

fn mapping_and_sending_system<T: Event + MapNetworkEntities + Serialize + Debug>(
    mut events: ResMut<Events<T>>,
    mut client: ResMut<RenetClient>,
    entity_map: Res<NetworkEntityMap>,
    channel: Res<EventChannel<T>>,
) {
    for mut event in events.drain() {
        event.map_entities(&mut EventMapper(entity_map.to_server()));
        let message = DefaultOptions::new()
            .serialize(&event)
            .expect("mapped client event should be serializable");
        client.send_message(channel.id, message);
        debug!("sent mapped client event {event:?}");
    }
}

fn sending_reflect_system<T, S>(
    mut events: EventReader<T>,
    mut client: ResMut<RenetClient>,
    channel: Res<EventChannel<T>>,
    registry: Res<AppTypeRegistry>,
) where
    T: Event + Debug,
    S: BuildEventSerializer<T>,
    for<'a> S::EventSerializer<'a>: Serialize,
{
    let registry = registry.read();
    for event in &mut events {
        let serializer = S::new(event, &registry);
        let message = DefaultOptions::new()
            .serialize(&serializer)
            .expect("client reflect event should be serializable");
        client.send_message(channel.id, message);
        debug!("sent client reflect event {event:?}");
    }
}

fn mapping_and_sending_reflect_system<T, S>(
    mut events: ResMut<Events<T>>,
    mut client: ResMut<RenetClient>,
    entity_map: Res<NetworkEntityMap>,
    channel: Res<EventChannel<T>>,
    registry: Res<AppTypeRegistry>,
) where
    T: Event + MapNetworkEntities + Debug,
    S: BuildEventSerializer<T>,
    for<'a> S::EventSerializer<'a>: Serialize,
{
    let registry = registry.read();
    for mut event in events.drain() {
        event.map_entities(&mut EventMapper(entity_map.to_server()));
        let serializer = S::new(&event, &registry);
        let message = DefaultOptions::new()
            .serialize(&serializer)
            .expect("mapped client reflect event should be serializable");
        client.send_message(channel.id, message);
        debug!("sent mapped client reflect event {event:?}");
    }
}

/// Transforms `T` events into [`FromClient<T>`] events to "emulate"
/// message sending for offline mode or when server is also a player
fn local_resending_system<T: Event + Debug>(
    mut events: ResMut<Events<T>>,
    mut client_events: EventWriter<FromClient<T>>,
) {
    for event in events.drain() {
        debug!("converted client event {event:?} into a local");
        client_events.send(FromClient {
            client_id: SERVER_ID,
            event,
        })
    }
}

/// An event indicating that a message from client was received.
/// Emited only on server.
#[derive(Clone, Copy, Event)]
pub struct FromClient<T> {
    pub client_id: u64,
    pub event: T,
}
