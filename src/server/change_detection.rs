use std::fmt;

use crate::replicon_core::replication_rules::{Replication, ReplicationId, ReplicationInfo};

use super::*;
use bevy::{
    ecs::component::{ComponentId, ComponentTicks},
    prelude::*,
    ptr::Ptr,
};

pub(super) struct ComponentChangeCandidate<'a> {
    pub replication_id: ReplicationId,
    pub replication_info: &'a ReplicationInfo,
    pub component_ptr: Ptr<'a>,
    pub component_ticks: ComponentTicks,
}

impl fmt::Debug for ComponentChangeCandidate<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}/{}@{:?}",
            self.replication_id,
            self.replication_info.type_name,
            self.component_ticks.last_changed_tick(),
        )
    }
}

pub(super) struct ComponentRemovalCandidate {
    pub replication_id: ReplicationId,
    /// tick at which the component was removed from entity
    pub tick: Tick,
}

impl fmt::Debug for ComponentRemovalCandidate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}@{:?}", self.replication_id, self.tick)
    }
}

pub(super) struct EntityCandidate<'a> {
    pub entity: Entity,
    pub replication_component: &'a Replication,
    pub changed_component_candidates: Vec<ComponentChangeCandidate<'a>>,
    pub removed_component_candidates: Vec<ComponentRemovalCandidate>,
}

impl fmt::Debug for EntityCandidate<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EntityCandidate{{{:?}}}\n  Changes:{:?}\n  Removals:{:?}",
            self.entity, self.changed_component_candidates, self.removed_component_candidates,
        )
    }
}

/// Get a vec of EntityCandidates for clients to consider
/// typically called with the oldest tick of all clients, and then post filter per client.
pub(super) fn collect_candidates<'a>(
    world: &'a World,
    replication_rules: &'a ReplicationRules,
    this_run: Tick,
    oldest_client_tick: Tick,
) -> Vec<EntityCandidate<'a>> {
    let removal_tracker_id = world
        .components()
        .component_id::<RemovalTracker>()
        .expect("RemovalTracker should exist on server");

    // this output array could be stored by clients and a ref passed in, so its capacity is already
    // mostly correct and allocated.
    let mut change_candidates = Vec::new();

    for archetype in world
        .archetypes()
        .iter()
        .filter(|archetype| archetype.id() != ArchetypeId::EMPTY)
        .filter(|archetype| archetype.id() != ArchetypeId::INVALID)
        .filter(|archetype| archetype.contains(replication_rules.get_marker_id()))
    {
        let table = world
            .storages()
            .tables
            .get(archetype.table_id())
            .expect("archetype should be valid");
        for archetype_entity in archetype.entities() {
            // extract the Replication component, which is a storage=table component.
            // all entities have this, we filtered on it above,
            //
            // Right now we don't need this, but we'll probably put rooms and/or priorities
            // into this component, so we'll need it to filter/sort candidates for sending.
            let col = table
                .get_column(replication_rules.get_marker_id())
                .expect("Already filtered on Replication component being present");
            let replication_component: &Replication = unsafe {
                // SAFETY: we definitely have the replication component, we filtered on it earlier.
                col.get_data_unchecked(archetype_entity.table_row()).deref()
            };

            // get component removals for this entity, from the RemovalTracker component.
            // all replicated entities get RemovalTracker, but there is a frame lag before the
            // insertion happens, so we guard against it being missing here:
            let removal_candidates = if archetype.contains(removal_tracker_id) {
                let col = table
                    .get_column(removal_tracker_id)
                    .expect("Already filtered on RemovalTracker component being present");
                let removal_tracker: &RemovalTracker = unsafe {
                    // SAFETY: we just confirmed we have the RemovalTracker component
                    col.get_data_unchecked(archetype_entity.table_row()).deref()
                };
                removal_tracker
                    .0
                    .iter()
                    .filter_map(|(&replication_id, &tick)| {
                        if tick.is_newer_than(oldest_client_tick, this_run) {
                            Some(ComponentRemovalCandidate {
                                replication_id,
                                tick,
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            // yield any components that:
            // * Are registered as replicated
            // * Aren't Ignored<>
            // * Have changed more recently than the oldest tick our clients collectively care about
            //
            let filter_component_candidate =
                |component_id: ComponentId| -> Option<ComponentChangeCandidate> {
                    let Some((replication_id, replication_info)) = replication_rules.get(component_id)
                    else {
                        return None;
                    };
                    if archetype.contains(replication_info.ignored_id) {
                        return None;
                    }
                    let storage_type = archetype
                        .get_storage_type(component_id)
                        .unwrap_or_else(|| panic!("{component_id:?} be in archetype"));

                    match storage_type {
                        StorageType::Table => {
                            let column = table.get_column(component_id).unwrap_or_else(|| {
                                panic!("{component_id:?} should belong to table")
                            });
                            // SAFETY: the table row obtained from the world state.
                            let component_ticks =
                                unsafe { column.get_ticks_unchecked(archetype_entity.table_row()) };
                            if !component_ticks.is_changed(oldest_client_tick, this_run) {
                                return None;
                            }
                            // SAFETY: component obtained from the archetype.
                            let component_ptr =
                                unsafe { column.get_data_unchecked(archetype_entity.table_row()) };
                            Some(ComponentChangeCandidate {
                                replication_id,
                                replication_info,
                                component_ptr,
                                component_ticks,
                            })
                        }
                        StorageType::SparseSet => {
                            let sparse_set = world
                                .storages()
                                .sparse_sets
                                .get(component_id)
                                .unwrap_or_else(|| {
                                    panic!("{component_id:?} should be in sparse set")
                                });
                            let entity = archetype_entity.entity();
                            let component_ticks =
                                sparse_set.get_ticks(entity).unwrap_or_else(|| {
                                    panic!("{entity:?} should have {component_id:?}")
                                });
                            if !component_ticks.is_changed(oldest_client_tick, this_run) {
                                return None;
                            }
                            let component_ptr = sparse_set.get(entity).unwrap_or_else(|| {
                                panic!("{entity:?} should have {component_id:?}")
                            });
                            Some(ComponentChangeCandidate {
                                replication_id,
                                replication_info,
                                component_ptr,
                                component_ticks,
                            })
                        }
                    }
                };

            let component_candidates = archetype
                .components()
                .filter_map(filter_component_candidate)
                .collect::<Vec<_>>();

            let ent_candidate = EntityCandidate {
                entity: archetype_entity.entity(),
                replication_component,
                changed_component_candidates: component_candidates,
                removed_component_candidates: removal_candidates,
            };

            change_candidates.push(ent_candidate);
        }
    }
    change_candidates
}
