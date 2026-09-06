//! Token 用量与价格统计（R6）：每次 LLM 调用从响应 usage 精确取数，
//! 按设置的价格（$/1M tokens）换算成本；全局累计 + 会话累计；预算到量自动停。

use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Copy, Serialize, Deserialize, Debug)]
pub struct Usage {
    pub prompt: u64,
    pub completion: u64,
    pub calls: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt += other.prompt;
        self.completion += other.completion;
        self.calls += other.calls;
    }
}

/// 按价格（$/1M）算一笔调用的成本
pub fn cost_usd(u: &Usage, price_in: f64, price_out: f64) -> f64 {
    u.prompt as f64 / 1_000_000.0 * price_in + u.completion as f64 / 1_000_000.0 * price_out
}

pub fn fmt_usd(v: f64) -> String {
    if v == 0.0 {
        "$0".into()
    } else if v < 0.01 {
        format!("${:.5}", v)
    } else {
        format!("${:.4}", v)
    }
}

/// LLM 响应里的 usage 字段（OpenAI 兼容）
#[derive(Deserialize, Debug, Default)]
pub struct ApiUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}
