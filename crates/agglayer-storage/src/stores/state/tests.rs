use std::sync::Arc;

use agglayer_types::{Hash, LocalNetworkStateData, NetworkId};
use rstest::{fixture, rstest};

use crate::{
    columns::latest_settled_certificate_per_network::{
        LatestSettledCertificatePerNetworkColumn, SettledCertificate,
    },
    error::Error,
    storage::{state_db_cf_definitions, DB},
    stores::{state::StateStore, StateReader as _, StateWriter as _},
    tests::TempDBDir,
};

mod metadata;

#[test]
fn can_retrieve_list_of_network() {
    let tmp = TempDBDir::new();
    let db = Arc::new(DB::open_cf(tmp.path.as_path(), state_db_cf_definitions()).unwrap());
    let store = StateStore::new(db.clone());
    assert!(store.get_active_networks().unwrap().is_empty());

    db.put::<LatestSettledCertificatePerNetworkColumn>(
        &1.into(),
        &SettledCertificate([0; 32].into(), 0, 0, 0),
    )
    .expect("Unable to put certificate into storage");
    assert!(store.get_active_networks().unwrap().len() == 1);
}

fn equal_state(lhs: &LocalNetworkStateData, rhs: &LocalNetworkStateData) -> bool {
    // local exit tree
    assert_eq!(lhs.exit_tree.leaf_count, rhs.exit_tree.leaf_count);
    assert_eq!(lhs.exit_tree.get_root(), rhs.exit_tree.get_root());

    // balance tree
    assert_eq!(lhs.balance_tree.root, rhs.balance_tree.root);
    assert_eq!(lhs.balance_tree.tree, rhs.balance_tree.tree);

    // nullifier tree
    assert_eq!(lhs.nullifier_tree.root, rhs.nullifier_tree.root);
    assert_eq!(lhs.nullifier_tree.tree, rhs.nullifier_tree.tree);

    true
}

#[fixture]
fn network_id() -> NetworkId {
    0.into()
}

#[fixture]
fn store() -> StateStore {
    let tmp = TempDBDir::new();
    let db = Arc::new(DB::open_cf(tmp.path.as_path(), state_db_cf_definitions()).unwrap());

    StateStore::new(db.clone())
}

#[rstest]
fn can_handle_empty_state(#[from(network_id)] unknown_network_id: NetworkId, store: StateStore) {
    // return none for unknown network
    assert!(matches!(
        store.read_local_network_state(unknown_network_id),
        Ok(None)
    ));

    // can write one state from scratch
    assert!(store
        .write_local_network_state(&unknown_network_id, &LocalNetworkStateData::default(), &[])
        .is_ok());
}

#[rstest]
fn can_retrieve_state(network_id: NetworkId, store: StateStore) {
    // write arbitrary state
    let mut lns = LocalNetworkStateData::default();
    let leaves = (0..10).map(|_| Hash([5u8; 32])).collect::<Vec<_>>();
    for l in &leaves {
        lns.exit_tree.add_leaf(l.0).unwrap();
    }

    assert!(store
        .write_local_network_state(&network_id, &lns, leaves.as_slice())
        .is_ok());

    // retrieve it
    assert!(
        matches!(store.read_local_network_state(network_id), Ok(Some(retrieved)) if equal_state(&lns, &retrieved))
    );
}

#[rstest]
fn can_update_existing_state(network_id: NetworkId, store: StateStore) {
    let mut lns = LocalNetworkStateData::default();

    // write initial state
    assert!(store
        .write_local_network_state(&network_id, &lns, &[])
        .is_ok());

    // update state
    let bridge_exit = [5u8; 32];
    lns.exit_tree.add_leaf(bridge_exit).unwrap();

    // write new state
    assert!(store
        .write_local_network_state(&network_id, &lns, &[Hash(bridge_exit)])
        .is_ok());

    // retrieve new state
    assert!(
        matches!(store.read_local_network_state(network_id), Ok(Some(retrieved)) if equal_state(&lns, &retrieved))
    );
}

#[rstest]
fn can_detect_inconsistent_state(network_id: NetworkId, store: StateStore) {
    let mut lns = LocalNetworkStateData::default();

    // write initial state
    assert!(store
        .write_local_network_state(&network_id, &lns, &[])
        .is_ok());

    // update state
    let bridge_exit = [5u8; 32];
    lns.exit_tree.add_leaf(bridge_exit).unwrap();

    // write new state with missing leaves
    assert!(matches!(
        store.write_local_network_state(&network_id, &lns, &[]),
        Err(Error::InconsistentState { .. })
    ));
}

#[rstest]
fn can_read() {
    let db_path = std::path::Path::new("/home/mhadji/agglayer/agglayer/data/");
    let db = Arc::new(DB::open_cf(&db_path.join("state"), state_db_cf_definitions()).unwrap());

    let store = StateStore::new(db.clone());
    let network_id: NetworkId = 15.into();
    let read_state = store.read_local_network_state(network_id);
    println!("{:?}", read_state);
    assert!(matches!(read_state, Ok(Some(_))));
}

use pessimistic_proof_test_suite::{
    forest::Forest,
    sample_data::{self as data},
};

#[rstest]
fn can_fail_state(network_id: NetworkId, store: StateStore) {
    let cached = false;
    let forest: Forest = data::sample_state_01();

    // write initial non-empty state
    let state = forest.state_b;
    assert!(store
        .write_local_network_state(&network_id, &state, &[])
        .is_ok());

    // insertion into balance smt
    

    // store again
    let read_state = store.read_local_network_state_inner(network_id, cached);

    // read again

    println!("{:?}", read_state);
    assert!(matches!(read_state, Ok(Some(_))));
}
