//! Environment configuration for the `funds_transfer` binary.
//!
//! Loading goes through a lookup function so tests can pass a map instead of mutating the
//! process environment; [`load_config`] is the `std::env` wrapper used by the binary.

use std::{env, fmt};

use ethers::types::{Address, U256};

#[derive(Clone, Debug)]
pub struct Config {
    pub http_rpc_url: String,
    pub chain_id: u64,

    pub rounds_manager_addr: Address,
    pub bonding_manager_addr: Address,

    pub json_key_file: String,
    pub passphrase_file: String,
    pub orchestrator_addr: Option<Address>,

    // Loop timing
    pub loop_sleep_secs: u64,
    // Tx receipt wait timeout
    pub receipt_timeout_secs: u64,

    // Fee pricing: maxFeePerGas = baseFee * multiplier, tip = max_priority_fee_wei
    pub max_fee_base_multiplier: u64,
    pub max_priority_fee_wei: U256,

    // Reward call (optional)
    pub enable_reward: bool,

    // Bond transfer (optional)
    pub enable_transfer_bond: bool,
    pub lpt_receiver_addr: Option<Address>,
    pub lpt_min_retain_wei: Option<U256>,

    // Fee withdrawal (optional)
    pub enable_withdraw_fees: bool,
    pub eth_fee_receiver_addr: Option<Address>,
    pub eth_fee_withdraw_threshold_wei: Option<U256>,
}

#[derive(Debug)]
pub enum AppError {
    MissingEnv(&'static str),
    BadEnv(&'static str, String),
    Provider(String),
    Wallet(String),
    Contract(String),
    Tx(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::MissingEnv(k) => write!(f, "missing env var: {k}"),
            AppError::BadEnv(k, v) => write!(f, "invalid env var {k}: {v}"),
            AppError::Provider(e) => write!(f, "provider error: {e}"),
            AppError::Wallet(e) => write!(f, "wallet error: {e}"),
            AppError::Contract(e) => write!(f, "contract error: {e}"),
            AppError::Tx(e) => write!(f, "tx error: {e}"),
        }
    }
}

impl std::error::Error for AppError {}

/// Returns the value of an env var, or `None` if it is unset (or not valid unicode).
type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn validate_config(cfg: &Config) -> Result<(), AppError> {
    if cfg.enable_transfer_bond {
        if cfg.lpt_receiver_addr.is_none() {
            return Err(AppError::BadEnv(
                "LPT_RECEIVER_ADDR",
                "required when ENABLE_TRANSFER_BOND=true".into(),
            ));
        }
        if cfg.lpt_min_retain_wei.is_none() {
            return Err(AppError::BadEnv(
                "LPT_MIN_RETAIN_WEI",
                "required when ENABLE_TRANSFER_BOND=true".into(),
            ));
        }
    }

    if cfg.enable_withdraw_fees {
        if cfg.eth_fee_receiver_addr.is_none() {
            return Err(AppError::BadEnv(
                "ETH_FEE_RECEIVER_ADDR",
                "required when ENABLE_WITHDRAW_FEES=true".into(),
            ));
        }
        if cfg.eth_fee_withdraw_threshold_wei.is_none() {
            return Err(AppError::BadEnv(
                "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
                "required when ENABLE_WITHDRAW_FEES=true".into(),
            ));
        }
    }

    Ok(())
}

/// Loads the config from the process environment.
pub fn load_config() -> Result<Config, AppError> {
    load_config_from(|key| env::var(key).ok())
}

/// Loads the config from `lookup`, which returns an env var's value or `None` if unset.
pub fn load_config_from(lookup: impl Fn(&str) -> Option<String>) -> Result<Config, AppError> {
    let env: Lookup = &lookup;

    // feature flags: default to current behavior (enabled) if not specified
    let enable_reward = parse_env_bool_opt(env, "ENABLE_REWARD")?.unwrap_or(true);
    let enable_transfer_bond = parse_env_bool_opt(env, "ENABLE_TRANSFER_BOND")?.unwrap_or(true);
    let enable_withdraw_fees = parse_env_bool_opt(env, "ENABLE_WITHDRAW_FEES")?.unwrap_or(true);

    let http_rpc_url = must_env(env, "HTTP_RPC_URL")?;
    let chain_id = must_parse_env_u64(env, "CHAIN_ID")?;

    let rounds_manager_addr = must_parse_env_addr(env, "ROUNDS_MANAGER_ADDR")?;
    let bonding_manager_addr = must_parse_env_addr(env, "BONDING_MANAGER_ADDR")?;

    let json_key_file = must_env(env, "JSON_KEY_FILE")?;
    let passphrase_file = must_env(env, "PASSPHRASE_FILE")?;
    let orchestrator_addr = parse_env_addr_opt(env, "ORCHESTRATOR_ADDR")?;

    let loop_sleep_secs = parse_env_u64_opt(env, "LOOP_SLEEP_SECS")?.unwrap_or(6);
    let receipt_timeout_secs = parse_env_u64_opt(env, "RECEIPT_TIMEOUT_SECS")?.unwrap_or(90);

    let max_fee_base_multiplier = parse_env_u64_opt(env, "MAX_FEE_BASE_MULTIPLIER")?.unwrap_or(2);
    if max_fee_base_multiplier == 0 {
        return Err(AppError::BadEnv(
            "MAX_FEE_BASE_MULTIPLIER",
            "must be >= 1".into(),
        ));
    }
    let max_priority_fee_wei = match env("MAX_PRIORITY_FEE_WEI") {
        Some(_) => must_parse_env_u256(env, "MAX_PRIORITY_FEE_WEI")?,
        None => U256::zero(),
    };

    let (lpt_receiver_addr, lpt_min_retain_wei) = if enable_transfer_bond {
        (
            Some(must_parse_env_addr(env, "LPT_RECEIVER_ADDR")?),
            Some(must_parse_env_u256(env, "LPT_MIN_RETAIN_WEI")?),
        )
    } else {
        (None, None)
    };

    let (eth_fee_receiver_addr, eth_fee_withdraw_threshold_wei) = if enable_withdraw_fees {
        (
            Some(must_parse_env_addr(env, "ETH_FEE_RECEIVER_ADDR")?),
            Some(must_parse_env_u256(env, "ETH_FEE_WITHDRAW_THRESHOLD_WEI")?),
        )
    } else {
        (None, None)
    };

    Ok(Config {
        http_rpc_url,
        chain_id,
        rounds_manager_addr,
        bonding_manager_addr,
        json_key_file,
        passphrase_file,
        orchestrator_addr,
        loop_sleep_secs,
        receipt_timeout_secs,
        max_fee_base_multiplier,
        max_priority_fee_wei,
        enable_reward,
        enable_transfer_bond,
        enable_withdraw_fees,
        lpt_receiver_addr,
        lpt_min_retain_wei,
        eth_fee_receiver_addr,
        eth_fee_withdraw_threshold_wei,
    })
}

fn must_env(env: Lookup, key: &'static str) -> Result<String, AppError> {
    env(key).ok_or(AppError::MissingEnv(key))
}

fn parse_env_u64_opt(env: Lookup, key: &'static str) -> Result<Option<u64>, AppError> {
    match env(key) {
        Some(s) => {
            let v = s
                .parse::<u64>()
                .map_err(|e| AppError::BadEnv(key, format!("{e}")))?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

fn must_parse_env_u64(env: Lookup, key: &'static str) -> Result<u64, AppError> {
    let s = must_env(env, key)?;
    s.parse::<u64>()
        .map_err(|e| AppError::BadEnv(key, format!("{e}")))
}

fn parse_env_addr_opt(env: Lookup, key: &'static str) -> Result<Option<Address>, AppError> {
    match env(key) {
        Some(s) => {
            let a = s
                .parse::<Address>()
                .map_err(|e| AppError::BadEnv(key, format!("{e}")))?;
            Ok(Some(a))
        }
        None => Ok(None),
    }
}

fn must_parse_env_addr(env: Lookup, key: &'static str) -> Result<Address, AppError> {
    let s = must_env(env, key)?;
    s.parse::<Address>()
        .map_err(|e| AppError::BadEnv(key, format!("{e}")))
}

fn must_parse_env_u256(env: Lookup, key: &'static str) -> Result<U256, AppError> {
    let s = must_env(env, key)?;
    // from_dec_str("") is Ok(0); an empty LPT_MIN_RETAIN_WEI would transfer the whole stake.
    if s.is_empty() {
        return Err(AppError::BadEnv(key, "must not be empty".into()));
    }
    // Accept decimal strings
    U256::from_dec_str(&s).map_err(|e| AppError::BadEnv(key, format!("{e}")))
}

fn parse_env_bool_opt(env: Lookup, key: &'static str) -> Result<Option<bool>, AppError> {
    match env(key) {
        Some(raw) => {
            let v = match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "y" | "on" => true,
                "0" | "false" | "no" | "n" | "off" => false,
                _ => {
                    return Err(AppError::BadEnv(
                        key,
                        "expected boolean (true/false)".into(),
                    ));
                }
            };
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;

    const ROUNDS: &str = "0xdd6f56DcC28D3F5f27084381fE8Df634985cc39f";
    const BONDING: &str = "0x35Bcf3c30594191d53231E4FF333E8A770453e40";
    const ORCH: &str = "0x0000000000000000000000000000000000000001";
    const LPT_RECEIVER: &str = "0x0000000000000000000000000000000000000002";
    const ETH_RECEIVER: &str = "0x0000000000000000000000000000000000000003";

    /// Every var a fully enabled config needs, and nothing optional.
    fn required() -> HashMap<&'static str, String> {
        [
            ("HTTP_RPC_URL", "http://localhost:8545"),
            ("CHAIN_ID", "42161"),
            ("ROUNDS_MANAGER_ADDR", ROUNDS),
            ("BONDING_MANAGER_ADDR", BONDING),
            ("JSON_KEY_FILE", "/keys/key.json"),
            ("PASSPHRASE_FILE", "/keys/pass.txt"),
            ("LPT_RECEIVER_ADDR", LPT_RECEIVER),
            ("LPT_MIN_RETAIN_WEI", "1000"),
            ("ETH_FEE_RECEIVER_ADDR", ETH_RECEIVER),
            ("ETH_FEE_WITHDRAW_THRESHOLD_WEI", "2000"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect()
    }

    fn load(vars: &HashMap<&'static str, String>) -> Result<Config, AppError> {
        load_config_from(|k| vars.get(k).cloned())
    }

    fn with(
        mut vars: HashMap<&'static str, String>,
        key: &'static str,
        value: &str,
    ) -> HashMap<&'static str, String> {
        vars.insert(key, value.to_string());
        vars
    }

    fn without(
        mut vars: HashMap<&'static str, String>,
        key: &str,
    ) -> HashMap<&'static str, String> {
        vars.remove(key);
        vars
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    /// Asserts `BadEnv(key, _)` and returns the message.
    fn bad_env(res: Result<Config, AppError>, key: &str) -> String {
        match res {
            Err(AppError::BadEnv(k, msg)) if k == key => msg,
            other => panic!("expected BadEnv({key}), got {other:?}"),
        }
    }

    fn missing_env(res: Result<Config, AppError>) -> &'static str {
        match res {
            Err(AppError::MissingEnv(k)) => k,
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // Defaults and required vars
    // ---------------------------------------------------------------

    #[test]
    fn loads_required_vars_with_defaults() {
        let cfg = load(&required()).unwrap();
        assert_eq!(cfg.http_rpc_url, "http://localhost:8545");
        assert_eq!(cfg.chain_id, 42161);
        assert_eq!(cfg.rounds_manager_addr, addr(ROUNDS));
        assert_eq!(cfg.bonding_manager_addr, addr(BONDING));
        assert_eq!(cfg.json_key_file, "/keys/key.json");
        assert_eq!(cfg.passphrase_file, "/keys/pass.txt");
        assert_eq!(cfg.orchestrator_addr, None);
        assert_eq!(cfg.loop_sleep_secs, 6);
        assert_eq!(cfg.receipt_timeout_secs, 90);
        assert_eq!(cfg.max_fee_base_multiplier, 2);
        assert_eq!(cfg.max_priority_fee_wei, U256::zero());
        assert!(cfg.enable_reward);
        assert!(cfg.enable_transfer_bond);
        assert!(cfg.enable_withdraw_fees);
        assert_eq!(cfg.lpt_receiver_addr, Some(addr(LPT_RECEIVER)));
        assert_eq!(cfg.lpt_min_retain_wei, Some(U256::from(1000)));
        assert_eq!(cfg.eth_fee_receiver_addr, Some(addr(ETH_RECEIVER)));
        assert_eq!(cfg.eth_fee_withdraw_threshold_wei, Some(U256::from(2000)));
        validate_config(&cfg).unwrap();
    }

    #[test]
    fn loads_optional_overrides() {
        let vars = [
            ("ORCHESTRATOR_ADDR", ORCH),
            ("LOOP_SLEEP_SECS", "60"),
            ("RECEIPT_TIMEOUT_SECS", "0"),
            ("MAX_FEE_BASE_MULTIPLIER", "1"),
            ("MAX_PRIORITY_FEE_WEI", "1000000"),
        ]
        .into_iter()
        .fold(required(), |vars, (k, v)| with(vars, k, v));
        let cfg = load(&vars).unwrap();
        assert_eq!(cfg.orchestrator_addr, Some(addr(ORCH)));
        assert_eq!(cfg.loop_sleep_secs, 60);
        assert_eq!(cfg.receipt_timeout_secs, 0);
        assert_eq!(cfg.max_fee_base_multiplier, 1);
        assert_eq!(cfg.max_priority_fee_wei, U256::from(1_000_000));
    }

    #[test]
    fn each_required_var_is_reported_when_missing() {
        for key in [
            "HTTP_RPC_URL",
            "CHAIN_ID",
            "ROUNDS_MANAGER_ADDR",
            "BONDING_MANAGER_ADDR",
            "JSON_KEY_FILE",
            "PASSPHRASE_FILE",
            "LPT_RECEIVER_ADDR",
            "LPT_MIN_RETAIN_WEI",
            "ETH_FEE_RECEIVER_ADDR",
            "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
        ] {
            assert_eq!(missing_env(load(&without(required(), key))), key);
        }
    }

    #[test]
    fn empty_environment_reports_first_required_var() {
        assert_eq!(missing_env(load(&HashMap::new())), "HTTP_RPC_URL");
    }

    #[test]
    fn invalid_feature_flag_is_reported_before_missing_vars() {
        let vars = with(HashMap::new(), "ENABLE_WITHDRAW_FEES", "maybe");
        bad_env(load(&vars), "ENABLE_WITHDRAW_FEES");
    }

    // ---------------------------------------------------------------
    // Feature flags
    // ---------------------------------------------------------------

    #[test]
    fn bool_flags_accept_every_spelling() {
        let truthy = ["1", "true", "yes", "y", "on", "TRUE", "Yes", " on ", "Y\n"];
        let falsy = [
            "0", "false", "no", "n", "off", "FALSE", "No", " off ", "\tN",
        ];
        for (raw, expected) in truthy
            .iter()
            .map(|r| (r, true))
            .chain(falsy.iter().map(|r| (r, false)))
        {
            for key in [
                "ENABLE_REWARD",
                "ENABLE_TRANSFER_BOND",
                "ENABLE_WITHDRAW_FEES",
            ] {
                let cfg = load(&with(required(), key, raw)).unwrap();
                let got = match key {
                    "ENABLE_REWARD" => cfg.enable_reward,
                    "ENABLE_TRANSFER_BOND" => cfg.enable_transfer_bond,
                    _ => cfg.enable_withdraw_fees,
                };
                assert_eq!(got, expected, "{key}={raw:?}");
            }
        }
    }

    #[test]
    fn bool_flags_reject_other_values() {
        for raw in ["", " ", "2", "enabled", "t", "f", "truee", "-1"] {
            for key in [
                "ENABLE_REWARD",
                "ENABLE_TRANSFER_BOND",
                "ENABLE_WITHDRAW_FEES",
            ] {
                let msg = bad_env(load(&with(required(), key, raw)), key);
                assert_eq!(msg, "expected boolean (true/false)");
            }
        }
    }

    #[test]
    fn disabled_transfer_bond_ignores_its_vars() {
        let vars = with(required(), "ENABLE_TRANSFER_BOND", "false");
        let vars = without(without(vars, "LPT_RECEIVER_ADDR"), "LPT_MIN_RETAIN_WEI");
        let cfg = load(&vars).unwrap();
        assert!(!cfg.enable_transfer_bond);
        assert_eq!(cfg.lpt_receiver_addr, None);
        assert_eq!(cfg.lpt_min_retain_wei, None);

        // Invalid values are not even parsed while disabled.
        let vars = with(required(), "ENABLE_TRANSFER_BOND", "off");
        let vars = with(vars, "LPT_RECEIVER_ADDR", "not-an-address");
        assert_eq!(load(&vars).unwrap().lpt_receiver_addr, None);
        validate_config(&cfg).unwrap();
    }

    #[test]
    fn disabled_withdraw_fees_ignores_its_vars() {
        let vars = with(required(), "ENABLE_WITHDRAW_FEES", "0");
        let vars = without(
            without(vars, "ETH_FEE_RECEIVER_ADDR"),
            "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
        );
        let cfg = load(&vars).unwrap();
        assert!(!cfg.enable_withdraw_fees);
        assert_eq!(cfg.eth_fee_receiver_addr, None);
        assert_eq!(cfg.eth_fee_withdraw_threshold_wei, None);

        let vars = with(required(), "ENABLE_WITHDRAW_FEES", "no");
        let vars = with(vars, "ETH_FEE_WITHDRAW_THRESHOLD_WEI", "1.5");
        assert_eq!(load(&vars).unwrap().eth_fee_withdraw_threshold_wei, None);
        validate_config(&cfg).unwrap();
    }

    #[test]
    fn reward_flag_does_not_change_required_vars() {
        let cfg = load(&with(required(), "ENABLE_REWARD", "false")).unwrap();
        assert!(!cfg.enable_reward);
        assert!(cfg.enable_transfer_bond && cfg.enable_withdraw_fees);
    }

    #[test]
    fn all_actions_disabled_needs_only_core_vars() {
        let vars = [
            "LPT_RECEIVER_ADDR",
            "LPT_MIN_RETAIN_WEI",
            "ETH_FEE_RECEIVER_ADDR",
            "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
        ]
        .into_iter()
        .fold(required(), without);
        let vars = with(vars, "ENABLE_TRANSFER_BOND", "n");
        let vars = with(vars, "ENABLE_WITHDRAW_FEES", "off");
        let vars = with(vars, "ENABLE_REWARD", "0");
        let cfg = load(&vars).unwrap();
        assert!(!cfg.enable_reward && !cfg.enable_transfer_bond && !cfg.enable_withdraw_fees);
        validate_config(&cfg).unwrap();
    }

    // ---------------------------------------------------------------
    // Value parsing
    // ---------------------------------------------------------------

    #[test]
    fn bad_u64_values_are_rejected() {
        for key in [
            "CHAIN_ID",
            "LOOP_SLEEP_SECS",
            "RECEIPT_TIMEOUT_SECS",
            "MAX_FEE_BASE_MULTIPLIER",
        ] {
            for raw in ["", "abc", "-1", "1.5", " 6", "0x10", "18446744073709551616"] {
                bad_env(load(&with(required(), key, raw)), key);
            }
        }
        let msg = bad_env(load(&with(required(), "CHAIN_ID", "abc")), "CHAIN_ID");
        assert_eq!(msg, "invalid digit found in string");
    }

    #[test]
    fn bad_addresses_are_rejected() {
        for key in [
            "ROUNDS_MANAGER_ADDR",
            "BONDING_MANAGER_ADDR",
            "ORCHESTRATOR_ADDR",
            "LPT_RECEIVER_ADDR",
            "ETH_FEE_RECEIVER_ADDR",
        ] {
            for raw in [
                "",
                "0x1234",
                "hello",
                "0xZZ6f56DcC28D3F5f27084381fE8Df634985cc39f",
            ] {
                bad_env(load(&with(required(), key, raw)), key);
            }
        }
    }

    #[test]
    fn addresses_parse_without_0x_prefix_and_any_case() {
        let vars = with(required(), "ORCHESTRATOR_ADDR", &ROUNDS[2..].to_lowercase());
        assert_eq!(load(&vars).unwrap().orchestrator_addr, Some(addr(ROUNDS)));
    }

    #[test]
    fn bad_u256_values_are_rejected() {
        let too_big = format!("{}0", U256::MAX);
        for key in [
            "MAX_PRIORITY_FEE_WEI",
            "LPT_MIN_RETAIN_WEI",
            "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
        ] {
            for raw in [" ", "abc", "-1", "1.5", "1e18", "0x10", too_big.as_str()] {
                bad_env(load(&with(required(), key, raw)), key);
            }
        }
    }

    #[test]
    fn empty_u256_values_are_rejected() {
        // U256::from_dec_str("") is Ok(0); an empty LPT_MIN_RETAIN_WEI must not mean
        // "retain nothing" and transfer the whole stake.
        for key in [
            "MAX_PRIORITY_FEE_WEI",
            "LPT_MIN_RETAIN_WEI",
            "ETH_FEE_WITHDRAW_THRESHOLD_WEI",
        ] {
            let msg = bad_env(load(&with(required(), key, "")), key);
            assert_eq!(msg, "must not be empty");
        }
    }

    #[test]
    fn explicit_zero_u256_values_are_allowed() {
        let vars = with(required(), "LPT_MIN_RETAIN_WEI", "0");
        let vars = with(vars, "ETH_FEE_WITHDRAW_THRESHOLD_WEI", "0");
        let cfg = load(&vars).unwrap();
        assert_eq!(cfg.lpt_min_retain_wei, Some(U256::zero()));
        assert_eq!(cfg.eth_fee_withdraw_threshold_wei, Some(U256::zero()));
    }

    #[test]
    fn u256_values_accept_full_range() {
        let max = U256::MAX.to_string();
        let vars = with(required(), "LPT_MIN_RETAIN_WEI", &max);
        let vars = with(vars, "MAX_PRIORITY_FEE_WEI", "0");
        let cfg = load(&vars).unwrap();
        assert_eq!(cfg.lpt_min_retain_wei, Some(U256::MAX));
        assert_eq!(cfg.max_priority_fee_wei, U256::zero());
    }

    #[test]
    fn zero_fee_multiplier_is_rejected() {
        let msg = bad_env(
            load(&with(required(), "MAX_FEE_BASE_MULTIPLIER", "0")),
            "MAX_FEE_BASE_MULTIPLIER",
        );
        assert_eq!(msg, "must be >= 1");
    }

    // ---------------------------------------------------------------
    // validate_config
    // ---------------------------------------------------------------

    #[test]
    fn validate_requires_transfer_bond_settings_when_enabled() {
        let base = load(&required()).unwrap();

        let mut cfg = base.clone();
        cfg.lpt_receiver_addr = None;
        let err = validate_config(&cfg).unwrap_err();
        assert!(matches!(err, AppError::BadEnv("LPT_RECEIVER_ADDR", _)));
        assert_eq!(
            err.to_string(),
            "invalid env var LPT_RECEIVER_ADDR: required when ENABLE_TRANSFER_BOND=true"
        );

        let mut cfg = base.clone();
        cfg.lpt_min_retain_wei = None;
        let err = validate_config(&cfg).unwrap_err();
        assert!(matches!(err, AppError::BadEnv("LPT_MIN_RETAIN_WEI", _)));

        let mut cfg = base;
        cfg.enable_transfer_bond = false;
        cfg.lpt_receiver_addr = None;
        cfg.lpt_min_retain_wei = None;
        validate_config(&cfg).unwrap();
    }

    #[test]
    fn validate_requires_withdraw_fees_settings_when_enabled() {
        let base = load(&required()).unwrap();

        let mut cfg = base.clone();
        cfg.eth_fee_receiver_addr = None;
        let err = validate_config(&cfg).unwrap_err();
        assert!(matches!(err, AppError::BadEnv("ETH_FEE_RECEIVER_ADDR", _)));
        assert_eq!(
            err.to_string(),
            "invalid env var ETH_FEE_RECEIVER_ADDR: required when ENABLE_WITHDRAW_FEES=true"
        );

        let mut cfg = base.clone();
        cfg.eth_fee_withdraw_threshold_wei = None;
        let err = validate_config(&cfg).unwrap_err();
        assert!(matches!(
            err,
            AppError::BadEnv("ETH_FEE_WITHDRAW_THRESHOLD_WEI", _)
        ));

        let mut cfg = base;
        cfg.enable_withdraw_fees = false;
        cfg.eth_fee_receiver_addr = None;
        cfg.eth_fee_withdraw_threshold_wei = None;
        validate_config(&cfg).unwrap();
    }

    #[test]
    fn validate_checks_transfer_bond_before_withdraw_fees() {
        let mut cfg = load(&required()).unwrap();
        cfg.lpt_receiver_addr = None;
        cfg.eth_fee_receiver_addr = None;
        assert!(matches!(
            validate_config(&cfg),
            Err(AppError::BadEnv("LPT_RECEIVER_ADDR", _))
        ));
    }

    // ---------------------------------------------------------------
    // AppError
    // ---------------------------------------------------------------

    #[test]
    fn app_error_display() {
        let cases = [
            (
                AppError::MissingEnv("CHAIN_ID"),
                "missing env var: CHAIN_ID",
            ),
            (
                AppError::BadEnv("CHAIN_ID", "bad".into()),
                "invalid env var CHAIN_ID: bad",
            ),
            (AppError::Provider("p".into()), "provider error: p"),
            (AppError::Wallet("w".into()), "wallet error: w"),
            (AppError::Contract("c".into()), "contract error: c"),
            (AppError::Tx("t".into()), "tx error: t"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
        let boxed: Box<dyn std::error::Error> = Box::new(AppError::Tx("t".into()));
        assert_eq!(boxed.to_string(), "tx error: t");
    }

    #[test]
    fn std_env_wrapper_reads_process_environment() {
        // Only checks that load_config goes through std::env; it does not set anything, so it
        // is safe to run in parallel. CI/dev shells normally lack HTTP_RPC_URL.
        if std::env::var("HTTP_RPC_URL").is_err() {
            assert_eq!(missing_env(load_config()), "HTTP_RPC_URL");
        }
    }

    proptest! {
        #[test]
        fn any_u64_round_trips(n in any::<u64>()) {
            let cfg = load(&with(required(), "LOOP_SLEEP_SECS", &n.to_string())).unwrap();
            prop_assert_eq!(cfg.loop_sleep_secs, n);
        }

        #[test]
        fn any_u256_round_trips(limbs in any::<[u64; 4]>()) {
            let v = U256(limbs);
            let cfg = load(&with(required(), "ETH_FEE_WITHDRAW_THRESHOLD_WEI", &v.to_string())).unwrap();
            prop_assert_eq!(cfg.eth_fee_withdraw_threshold_wei, Some(v));
        }
    }
}
