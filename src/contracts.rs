//! Livepeer contract bindings. ABIs come from livepeer/protocol `deployments/arbitrumMainnet`.
//! BondingManager and RoundsManager sit behind proxies, so their ABIs are the `.abi` of the
//! `BondingManagerTarget.json` / `RoundsManagerTarget.json` artifacts; Minter and Controller use
//! their own artifacts. The BondingManager ABI (with the third-party bonder authorization
//! functions) is from branch `add-third-party-mapping`, matching the deployed target
//! `0xbE3B607B990e3987CA28188ED2e73F52ebbD3bd2`.
//!
//! Each `sol!` invocation generates a module named after the contract, holding the call types
//! and a `<Name>Instance<P>` wrapper; `<Name>::new(address, provider)` creates the instance.

// Generated: BondingManager.bondForWithHint takes seven arguments.
#![allow(clippy::too_many_arguments)]

use alloy::sol;

sol!(
    #[sol(rpc)]
    BondingManager,
    "src/abi/BondingManager.json"
);

sol!(
    #[sol(rpc)]
    RoundsManager,
    "src/abi/RoundsManager.json"
);

sol!(
    #[sol(rpc)]
    Minter,
    "src/abi/Minter.json"
);

sol!(
    #[sol(rpc)]
    Controller,
    "src/abi/Controller.json"
);

pub use BondingManager::BondingManagerInstance;
pub use Controller::ControllerInstance;
pub use Minter::MinterInstance;
pub use RoundsManager::RoundsManagerInstance;
