use base64::Engine;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Sha3_256};

#[derive(Debug, Deserialize)]
pub struct PowChallengeResponse {
    pub biz_data: PowBizData,
}

#[derive(Debug, Deserialize)]
pub struct PowBizData {
    pub challenge: PowChallengeData,
}

#[derive(Debug, Deserialize)]
pub struct PowChallengeData {
    pub algorithm: String,
    pub challenge: String,
    pub difficulty: String,
    pub salt: String,
    pub signature: String,
    pub target_path: String,
    pub expire_at: u64,
}

#[derive(Debug, Serialize)]
pub struct PowSolution {
    pub algorithm: String,
    pub answer: u64,
    pub challenge: String,
    pub difficulty: String,
    pub expire_at: u64,
    pub salt: String,
    pub signature: String,
    pub target_path: String,
}

pub fn solve_pow(challenge: &PowChallengeData) -> Option<PowSolution> {
    if challenge.algorithm != "DeepSeekHashV1" {
        tracing::warn!("Unknown PoW algorithm: {}", challenge.algorithm);
        return None;
    }

    let difficulty: f64 = challenge.difficulty.parse().ok()?;
    let prefix = format!("{}_{}_", challenge.salt, challenge.expire_at);

    let answer = find_nonce(&challenge.challenge, &prefix, difficulty)?;

    Some(PowSolution {
        algorithm: challenge.algorithm.clone(),
        answer,
        challenge: challenge.challenge.clone(),
        difficulty: challenge.difficulty.clone(),
        expire_at: challenge.expire_at,
        salt: challenge.salt.clone(),
        signature: challenge.signature.clone(),
        target_path: challenge.target_path.clone(),
    })
}

fn find_nonce(_challenge: &str, prefix: &str, difficulty: f64) -> Option<u64> {
    let max_nonce: u64 = 10_000_000;

    for nonce in 0..max_nonce {
        let input = format!("{prefix}{nonce}");
        let hash = sha3_256(input.as_bytes());

        if check_difficulty(&hash, difficulty) {
            return Some(nonce);
        }
    }

    tracing::warn!("PoW: failed to find nonce within {} attempts", max_nonce);
    None
}

fn sha3_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut output = [0u8; 32];
    output.copy_from_slice(&result);
    output
}

fn check_difficulty(hash: &[u8; 32], difficulty: f64) -> bool {
    let target = compute_target(difficulty);
    let hash_value = u64::from_be_bytes(hash[..8].try_into().unwrap());
    hash_value < target
}

fn compute_target(difficulty: f64) -> u64 {
    let max_value = u64::MAX as f64;
    (max_value / difficulty) as u64
}

pub fn encode_solution(solution: &PowSolution) -> String {
    let json = serde_json::to_string(solution).unwrap();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}
