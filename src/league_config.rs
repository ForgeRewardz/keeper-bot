// ============================================================
// league_config.rs — Rust mirror of sdk/packages/types/src/league.ts
//
// Every field must match the TypeScript shape exactly (name, type, ordering
// via serde). Values for `DEVNET` and `MAINNET` must equal their TS counterparts.
// CI parity test: api/tests/league-config-parity.test.ts dumps both as JSON
// and asserts deep-equal.
// ============================================================

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Devnet,
    Mainnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetCadence {
    Weekly,
    Yearly,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityWeights {
    #[serde(rename = "U")]
    pub u: f64,
    #[serde(rename = "R")]
    pub r: f64,
    #[serde(rename = "C")]
    pub c: f64,
    #[serde(rename = "S")]
    pub s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationMilestones {
    pub first_blink: u64,
    pub first_stake: u64,
    pub first_issuance: u64,
    pub first_five_unique_wallets: u64,
    pub first_repeat_user: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthMilestones {
    pub twenty_five_unique_wallets: u64,
    pub ten_repeat_users: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocialMilestones {
    pub follow: u64,
    pub launch_thread: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestones {
    pub activation: ActivationMilestones,
    pub growth: GrowthMilestones,
    pub social: SocialMilestones,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WalletWeights {
    pub founder: f64,
    pub team: f64,
    pub external: f64,
    pub external_repeat_after_gap: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyCaps {
    #[serde(rename = "self")]
    pub self_: u64,
    pub social: u64,
    pub referral: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RankingWeights {
    pub quality: f64,
    pub unique_wallets: f64,
    pub repeat_users: f64,
    pub completions: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LeagueConfig {
    pub network: Network,

    // Economy
    pub starter_grant_rewardz: u64,
    pub issuance_ratio: u64,
    pub capacity_reset_cadence: ResetCadence,
    pub capacity_warning_thresholds: [f64; 3],

    // Quality-score weights (must sum to 1.0)
    pub quality_weights: QualityWeights,

    // Milestones
    pub milestones: Milestones,

    // Anti-abuse
    pub wallet_weights: WalletWeights,
    pub repeat_gap_hours: u64,
    pub daily_caps: DailyCaps,

    // Publisher
    pub rewardz_publish_interval_secs: u64,
    pub max_rewardz_per_epoch: u64,

    // Visibility
    pub activity_window_hours: u64,
    pub inactivity_window_hours: u64,

    // Ranking
    pub ranking_weights: RankingWeights,

    // Leaderboard
    pub leaderboard_top_n: u64,
    pub leaderboard_bonus_rewardz: u64,
}

pub const DEVNET: LeagueConfig = LeagueConfig {
    network: Network::Devnet,

    starter_grant_rewardz: 100,
    issuance_ratio: 10,
    capacity_reset_cadence: ResetCadence::Weekly,
    capacity_warning_thresholds: [0.25, 0.1, 0.0],

    quality_weights: QualityWeights {
        u: 0.4,
        r: 0.3,
        c: 0.2,
        s: 0.1,
    },

    milestones: Milestones {
        activation: ActivationMilestones {
            first_blink: 100,
            first_stake: 100,
            first_issuance: 150,
            first_five_unique_wallets: 150,
            first_repeat_user: 100,
        },
        growth: GrowthMilestones {
            twenty_five_unique_wallets: 100,
            ten_repeat_users: 150,
        },
        social: SocialMilestones {
            follow: 10,
            launch_thread: 25,
        },
    },

    wallet_weights: WalletWeights {
        founder: 0.25,
        team: 0.5,
        external: 1.0,
        external_repeat_after_gap: 1.25,
    },
    repeat_gap_hours: 24,
    daily_caps: DailyCaps {
        self_: 25,
        social: 100,
        referral: 50,
    },

    rewardz_publish_interval_secs: 3600,
    max_rewardz_per_epoch: 10_000,

    activity_window_hours: 168,
    inactivity_window_hours: 336,

    ranking_weights: RankingWeights {
        quality: 0.5,
        unique_wallets: 0.2,
        repeat_users: 0.2,
        completions: 0.1,
    },

    leaderboard_top_n: 10,
    leaderboard_bonus_rewardz: 50,
};

// TODO(governance): mainnet values are placeholders until the governance
// session locks them. Do not deploy to mainnet while these are zero.
pub const MAINNET: LeagueConfig = LeagueConfig {
    network: Network::Mainnet,

    starter_grant_rewardz: 0,
    issuance_ratio: 0,
    capacity_reset_cadence: ResetCadence::Yearly,
    capacity_warning_thresholds: [0.0, 0.0, 0.0],

    quality_weights: QualityWeights {
        u: 0.0,
        r: 0.0,
        c: 0.0,
        s: 0.0,
    },

    milestones: Milestones {
        activation: ActivationMilestones {
            first_blink: 0,
            first_stake: 0,
            first_issuance: 0,
            first_five_unique_wallets: 0,
            first_repeat_user: 0,
        },
        growth: GrowthMilestones {
            twenty_five_unique_wallets: 0,
            ten_repeat_users: 0,
        },
        social: SocialMilestones {
            follow: 0,
            launch_thread: 0,
        },
    },

    wallet_weights: WalletWeights {
        founder: 0.0,
        team: 0.0,
        external: 0.0,
        external_repeat_after_gap: 0.0,
    },
    repeat_gap_hours: 0,
    daily_caps: DailyCaps {
        self_: 0,
        social: 0,
        referral: 0,
    },

    rewardz_publish_interval_secs: 0,
    max_rewardz_per_epoch: 0,

    activity_window_hours: 0,
    inactivity_window_hours: 0,

    ranking_weights: RankingWeights {
        quality: 0.0,
        unique_wallets: 0.0,
        repeat_users: 0.0,
        completions: 0.0,
    },

    leaderboard_top_n: 0,
    leaderboard_bonus_rewardz: 0,
};

pub fn load_league_config() -> LeagueConfig {
    // `localnet` reuses the devnet preset (standard practice for local
    // iteration — matches setup.sh's LEAGUE_NETWORK mapping). `mainnet-beta`
    // is accepted as an alias for `mainnet` because that's the canonical
    // cluster label in .env.shared / Solana CLI config.
    match std::env::var("SOLANA_NETWORK").as_deref() {
        Ok("devnet") | Ok("localnet") => DEVNET,
        Ok("mainnet") | Ok("mainnet-beta") => MAINNET,
        other => panic!(
            "Unknown SOLANA_NETWORK: {other:?} (expected devnet|localnet|mainnet|mainnet-beta)"
        ),
    }
}

/// Dump the resolved LeagueConfig for the current `SOLANA_NETWORK` env var as JSON.
/// Used by the CI parity test to diff against the TypeScript dump.
pub fn dump_json() -> String {
    let cfg = load_league_config();
    serde_json::to_string_pretty(&cfg).expect("LeagueConfig serialises")
}
