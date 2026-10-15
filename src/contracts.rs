//! Livepeer contract bindings. ABIs come from livepeer/protocol `deployments/arbitrumMainnet`.

use ethers::contract::abigen;

abigen!(
    BondingManager,
    "src/abi/BondingManager.json",
    event_derives(serde::Deserialize, serde::Serialize)
);

abigen!(
    RoundsManager,
    "src/abi/RoundsManager.json",
    event_derives(serde::Deserialize, serde::Serialize)
);

abigen!(
    Minter,
    "src/abi/Minter.json",
    event_derives(serde::Deserialize, serde::Serialize)
);

abigen!(
    Controller,
    "src/abi/Controller.json",
    event_derives(serde::Deserialize, serde::Serialize)
);
