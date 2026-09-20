//! Optional seed data (design §7.4): the user's own five-life-system framework,
//! offered at init time, never forced. Installing seeds Areas + Directions
//! through the SAME direct-write paths the HTTP layer uses (so it produces the
//! same events, same tables — no parallel "template" machinery to keep in
//! sync). Deleting/editing what it created afterward is ordinary Area/Direction
//! management; nothing here is special-cased to survive a delete.

use crate::core::Area;
use crate::store::{Result, Sin90Store};

/// One Area + its opening Directions, as bundled by [`five_life_systems`].
pub struct SeedArea {
    pub title: &'static str,
    pub directions: &'static [&'static str],
}

/// The user's own framework (design §1: 财富4账户/学习IN→OUT/工作3+90+复盘/
/// 健康睡眠-运动-饮食/关系三层), transcribed as starter Areas + Directions —
/// not hardcoded into any state machine or route, just data a fresh install
/// MAY choose to seed.
pub fn five_life_systems() -> Vec<SeedArea> {
    vec![
        SeedArea {
            title: "财富 Wealth",
            directions: &["4 账户结构：生活/投资/应急/主账户自动转账"],
        },
        SeedArea {
            title: "学习 Learning",
            directions: &["IN→OUT 闭环：每日阅读 + 每周输出"],
        },
        SeedArea {
            title: "工作 Work",
            directions: &["3+90+复盘：每日 3 件事 + 90 分钟深度工作 + 周五复盘"],
        },
        SeedArea {
            title: "健康 Health",
            directions: &["睡眠·运动·饮食三支柱"],
        },
        SeedArea {
            title: "关系 Relation",
            directions: &["核心/深层/外圈三层关系维护节奏"],
        },
    ]
}

impl Sin90Store {
    /// Install a bundle of seed Areas + Directions. Idempotent-ish in the loose
    /// sense that re-running it creates a SECOND set (Area titles are not
    /// unique) — this is intentional: `create_area` already dedupes only the
    /// URL-safe `slug`, and forcing true idempotency here would require a
    /// "pack identity" concept design §7.4 explicitly did not ask for. Callers
    /// (the HTTP handler) are expected to call this exactly once at
    /// first-run, which is the only place it is wired.
    pub async fn install_seed_pack(&self, pack: &[SeedArea]) -> Result<Vec<Area>> {
        let mut created = Vec::with_capacity(pack.len());
        for seed in pack {
            let area = self.create_area(seed.title).await?;
            for title in seed.directions {
                // Seed directions get a generic open-ended window; a real
                // Direction the user edits afterward like any other.
                self.create_direction(title, "ongoing", Some(&area.id))
                    .await?;
            }
            created.push(area);
        }
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn installing_the_five_life_systems_creates_five_areas_with_directions() {
        let store = Sin90Store::open_memory().await.unwrap();
        let created = store.install_seed_pack(&five_life_systems()).await.unwrap();
        assert_eq!(created.len(), 5);

        let areas = store.list_areas().await.unwrap();
        assert_eq!(areas.len(), 5);

        let directions = store.list_directions().await.unwrap();
        // Each seed area has exactly one opening direction in this bundle.
        assert_eq!(directions.len(), 5);
        assert!(directions.iter().all(|d| d.area_id.is_some()));
    }

    #[tokio::test]
    async fn seeded_areas_are_ordinary_areas_deletable_like_any_other() {
        // "Optional, editable, not hardcoded" (design §7.4) means: nothing
        // about a seeded Area is special. Archiving it goes through the exact
        // same transition_area path a hand-created Area would.
        let store = Sin90Store::open_memory().await.unwrap();
        let created = store.install_seed_pack(&five_life_systems()).await.unwrap();
        let first = &created[0];
        let archived = store
            .transition_area(&first.id, crate::core::AreaStatus::Archived)
            .await
            .unwrap();
        assert_eq!(archived.status, crate::core::AreaStatus::Archived);
    }
}
