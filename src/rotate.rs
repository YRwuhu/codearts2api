//! 多账号轮换策略。
//!
//! 早先的实现是「按配置文件顺序试，第一个健康就用」，这在多账号下有两个问题：
//!
//! 1. **不公平**：第一个账号会被打死，后面的账号几乎闲置，单账号的日额度上限
//!    和套餐 credit 很快被一个号吃光，其余号闲着。
//! 2. **不看余额**：账号 A 只剩 1% credit、账号 B 还有 100%，请求仍然先打 A。
//!
//! 现在的选号分两层：
//!
//! - **会话亲和优先**：同一对话已有钉号时直接用钉住的账号（prompt cache 是
//!   账号级的，换号等于丢掉缓存）。钉号账号若不健康，由 [`order`] 把它排到
//!   备选位置，`chat::run_for_model` 的失败切换会接手并重新钉号。
//! - **新会话按策略选号**：默认 `balanced`，先比在途负载（`in_flight/limit`），
//!   再比「最久没被选过」，这样空闲时会自然轮转，不会总压同一个号。
//!   `credit` 策略先比剩余 credit，适合「额度不均、想让大号先扛」的场景。
//!   `roundrobin` 只按最久未用排序，是最朴素的平均分配。
//!
//! 不可用的账号不会直接丢掉，而是排到可用账号之后作为兜底——上游的冷却
//! 经常是秒级抖动，直接返回 429 不如顶着冷却试一把。

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// 每账号的选号视图。由 [`crate::concurrency::AccountGate`] 填充。
#[derive(Clone, Debug, Default)]
pub struct AccountState {
    /// 该账号当前的流式在途请求数
    pub in_flight: u32,
    /// 该账号的本地并发上限
    pub limit: u32,
    /// 账号级冷却中（并发错误退避或连续失败）
    pub cooling: bool,
    /// 该模型在这个账号上冷却中
    pub model_cooling: bool,
    /// 该账号当日 token 已达上限
    pub daily_exhausted: bool,
    /// 距离上次被选中的毫秒数；`None` 表示本次进程内还没用过
    pub idle_ms: Option<u64>,
    /// 剩余 credit；额度接口没拿到时为 `None`
    pub credit_remain: Option<f64>,
    /// 剩余 credit 占比（0..=1）；与 `credit_remain` 同时为 `None` 或同时有值
    pub credit_ratio: Option<f64>,
    /// 连续失败次数，只作为同分时的次级排序依据
    pub consecutive_failures: u32,
}

/// 账号选择策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// 负载均衡：在途少的优先，空闲时按最久未用轮转。
    #[default]
    Balanced,
    /// 余额优先：剩余 credit 多的优先，适合套餐额度不均。
    Credit,
    /// 严格轮转：只按最久未用，平均分配请求数。
    RoundRobin,
}

impl Strategy {
    /// 从环境变量值解析；无法识别时回退默认策略。
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "credit" | "quota" | "balance" => Strategy::Credit,
            "roundrobin" | "round-robin" | "rr" | "rotate" => Strategy::RoundRobin,
            _ => Strategy::Balanced,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::Balanced => "balanced",
            Strategy::Credit => "credit",
            Strategy::RoundRobin => "roundrobin",
        }
    }

    /// 给 WebUI 展示的中文说明。
    pub fn label(&self) -> &'static str {
        match self {
            Strategy::Balanced => "负载均衡（在途少者优先，空闲轮转）",
            Strategy::Credit => "余额优先（剩余额度多者优先）",
            Strategy::RoundRobin => "严格轮转（按最久未用）",
        }
    }
}

/// 全局选号参数。
#[derive(Clone, Debug)]
pub struct Policy {
    pub strategy: Strategy,
    /// 低于该剩余 credit 的账号降级为兜底（0 表示不启用）。
    pub min_credit: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            strategy: Strategy::default(),
            min_credit: 0.0,
        }
    }
}

impl Policy {
    /// 账号是否「余额不足」，应当让位给余额更充足的账号。
    ///
    /// `min_credit <= 0` 表示不启用该阈值，此时一律视为余额充足。
    fn credit_blocked(&self, state: &AccountState) -> bool {
        if self.min_credit <= 0.0 {
            return false;
        }
        state
            .credit_remain
            .map(|remain| remain < self.min_credit)
            .unwrap_or(false)
    }
}

/// 排序层级：数字越小越先用。
///
/// 冷却和额度耗尽的账号排在后面而不是被剔除——上游冷却常有秒级抖动，
/// 全都「不可用」时顶着试一把，好过直接给客户端 429。
fn tier(policy: &Policy, state: &AccountState) -> u8 {
    if state.cooling {
        return 3;
    }
    if state.model_cooling {
        return 2;
    }
    if state.daily_exhausted || policy.credit_blocked(state) {
        return 1;
    }
    0
}

/// 在途负载比。limit 为 0 时视为满载，避免除零。
fn load(state: &AccountState) -> f64 {
    if state.limit == 0 {
        return 1.0;
    }
    state.in_flight as f64 / state.limit as f64
}

/// 比较两个账号的优先级，`Less` 表示 a 应该排在 b 前面。
///
/// 同层内的比较顺序刻意保持「先主指标、后公平性」：
/// 主指标相同的账号再按最久未用排序，因此空闲时就是轮转。
fn compare(policy: &Policy, a: &AccountState, b: &AccountState) -> Ordering {
    let (ta, tb) = (tier(policy, a), tier(policy, b));
    if ta != tb {
        return ta.cmp(&tb);
    }

    // 主指标
    match policy.strategy {
        Strategy::Credit => {
            // 有额度信息的排在没有的前面（信息缺失说明还没刷新过额度）
            match (a.credit_ratio, b.credit_ratio) {
                (Some(x), Some(y)) => {
                    let ord = y.partial_cmp(&x).unwrap_or(Ordering::Equal);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => {}
            }
            let ord = load(a).partial_cmp(&load(b)).unwrap_or(Ordering::Equal);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Strategy::Balanced | Strategy::RoundRobin => {
            if policy.strategy == Strategy::Balanced {
                let ord = load(a).partial_cmp(&load(b)).unwrap_or(Ordering::Equal);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }

    // 公平性：最久没被选过的优先；从没用过排在最前。
    match (a.idle_ms, b.idle_ms) {
        (Some(x), Some(y)) => {
            let ord = y.cmp(&x);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        (None, Some(_)) => return Ordering::Less,
        (Some(_), None) => return Ordering::Greater,
        (None, None) => {}
    }

    // 失败次数少的优先，最后用配置顺序兜底，保证排序稳定。
    a.consecutive_failures.cmp(&b.consecutive_failures)
}

/// 返回候选账号的下标顺序（升序优先级）。下标对应调用方传入的切片顺序。
pub fn order(policy: &Policy, states: &[AccountState]) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..states.len()).collect();
    indices.sort_by(|&i, &j| compare(policy, &states[i], &states[j]).then_with(|| i.cmp(&j)));
    indices
}

/// 按策略排序后的账号下标，同时把 `pinned` 提到最前（会话亲和）。
///
/// 钉号账号即使被排到兜底层，也仍然排在第一——请求先在原账号上试，
/// 失败后由 `run_for_model` 的切换逻辑接手。这样既保住 prompt cache，
/// 又不会因为钉在一个坏号上而让整段对话卡死。
pub fn order_with_pin(
    policy: &Policy,
    states: &[AccountState],
    pinned: Option<usize>,
) -> Vec<usize> {
    let mut ordered = order(policy, states);
    if let Some(pin) = pinned {
        if let Some(pos) = ordered.iter().position(|&i| i == pin) {
            ordered.remove(pos);
            ordered.insert(0, pin);
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(load: u32, limit: u32, idle_ms: Option<u64>) -> AccountState {
        AccountState {
            in_flight: load,
            limit,
            idle_ms,
            ..Default::default()
        }
    }

    #[test]
    fn parses_strategy_aliases() {
        assert_eq!(Strategy::parse("credit"), Strategy::Credit);
        assert_eq!(Strategy::parse(" ROUND-ROBIN "), Strategy::RoundRobin);
        assert_eq!(Strategy::parse("rr"), Strategy::RoundRobin);
        assert_eq!(Strategy::parse(""), Strategy::Balanced);
        assert_eq!(Strategy::parse("nonsense"), Strategy::Balanced);
    }

    #[test]
    fn balanced_picks_the_least_loaded_account() {
        let policy = Policy::default();
        let states = [
            state(2, 4, Some(1000)),
            state(0, 4, Some(10)),
            state(1, 4, None),
        ];
        assert_eq!(order(&policy, &states), vec![1, 2, 0]);
    }

    #[test]
    fn balanced_ties_break_by_least_recently_used() {
        let policy = Policy::default();
        // 三个号都空闲：先选从没用过的，再按最久没用的顺序
        let states = [
            state(0, 4, Some(5000)),
            state(0, 4, None),
            state(0, 4, Some(9000)),
        ];
        assert_eq!(order(&policy, &states), vec![1, 2, 0]);
    }

    #[test]
    fn ordering_rotates_after_each_selection() {
        let policy = Policy::default();
        // 模拟真实时钟：idle_ms = 现在 - 上次被选中的时刻。
        // 未被选过的账号 idle 记为 None（最优先）。
        let mut last_used: [Option<u64>; 3] = [None, None, None];
        let mut picked = Vec::new();
        for tick in 0..6u64 {
            let states: Vec<AccountState> = last_used
                .iter()
                .map(|seen| AccountState {
                    limit: 4,
                    idle_ms: seen.map(|t| (tick - t) * 100),
                    ..Default::default()
                })
                .collect();
            let next = order(&policy, &states)[0];
            picked.push(next);
            last_used[next] = Some(tick);
        }
        assert_eq!(picked, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn cooling_accounts_sink_below_healthy_ones_but_stay_in_list() {
        let policy = Policy::default();
        let mut hot = state(0, 4, Some(9000));
        hot.cooling = true;
        let states = [hot, state(3, 4, None)];
        // 冷却中的号负载看起来最轻，但仍必须排在后面
        assert_eq!(order(&policy, &states), vec![1, 0]);
        assert_eq!(order(&policy, &states).len(), 2);
    }

    #[test]
    fn model_cooldown_is_softer_than_account_cooldown() {
        let policy = Policy::default();
        let mut model_cool = state(0, 4, Some(9000));
        model_cool.model_cooling = true;
        let mut account_cool = state(0, 4, Some(9000));
        account_cool.cooling = true;
        let states = [account_cool, model_cool];
        assert_eq!(order(&policy, &states), vec![1, 0]);
    }

    #[test]
    fn credit_strategy_prefers_richer_account() {
        let policy = Policy {
            strategy: Strategy::Credit,
            min_credit: 0.0,
        };
        let mut rich = state(2, 4, Some(9000));
        rich.credit_ratio = Some(0.9);
        let mut poor = state(0, 4, None);
        poor.credit_ratio = Some(0.05);
        let states = [poor, rich];
        assert_eq!(order(&policy, &states), vec![1, 0]);
    }

    #[test]
    fn min_credit_lets_fresh_accounts_jump_ahead() {
        let policy = Policy {
            strategy: Strategy::Credit,
            min_credit: 100.0,
        };
        let mut empty = state(0, 4, None);
        empty.credit_remain = Some(5.0);
        let mut fresh = state(3, 4, Some(9000));
        fresh.credit_remain = Some(5000.0);
        let states = [empty, fresh];
        // 余额不足的号即使更闲也排到后面
        assert_eq!(order(&policy, &states), vec![1, 0]);
    }

    #[test]
    fn missing_credit_info_does_not_panic_or_reorder_randomly() {
        let policy = Policy {
            strategy: Strategy::Credit,
            min_credit: 10.0,
        };
        let states = [state(0, 4, Some(100)), state(0, 4, None)];
        // 两个都没有余额信息：按公平性（未用过的优先）
        assert_eq!(order(&policy, &states), vec![1, 0]);
    }

    #[test]
    fn daily_exhausted_account_is_deprioritized_but_kept() {
        let policy = Policy::default();
        let mut spent = state(0, 4, None);
        spent.daily_exhausted = true;
        let states = [spent, state(4, 4, Some(9000))];
        assert_eq!(order(&policy, &states), vec![1, 0]);
    }

    #[test]
    fn pin_moves_to_front_without_dropping_others() {
        let policy = Policy::default();
        let states = [
            state(3, 4, None),
            state(0, 4, Some(100)),
            state(1, 4, Some(200)),
        ];
        assert_eq!(order_with_pin(&policy, &states, Some(2)), vec![2, 1, 0]);
        assert_eq!(order_with_pin(&policy, &states, None), vec![1, 2, 0]);
        // 越界或已不存在的钉号被忽略
        assert_eq!(order_with_pin(&policy, &states, Some(9)), vec![1, 2, 0]);
    }
}
