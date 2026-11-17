//! Scope scenario catalogs to declared installations while retaining real reads.
use std::collections::HashSet;
use std::sync::Arc;

use codex_exec_server::EnvironmentManager;
use codex_skills_extension::ExecutorSkillProvider;
use codex_skills_extension::SkillProvider;
use codex_skills_extension::catalog::SkillCatalog;
use codex_skills_extension::catalog::SkillReadResult;
use codex_skills_extension::catalog::SkillSearchResult;
use codex_skills_extension::provider::SkillListQuery;
use codex_skills_extension::provider::SkillProviderFuture;
use codex_skills_extension::provider::SkillReadRequest;
use codex_skills_extension::provider::SkillSearchRequest;

pub(super) struct FixtureExecutorSkills {
    inner: ExecutorSkillProvider,
    allowed: HashSet<String>,
}

impl FixtureExecutorSkills {
    pub(super) fn new(allowed: &[&str]) -> Self {
        Self {
            inner: ExecutorSkillProvider::new_with_restriction_product(
                Arc::new(EnvironmentManager::default_for_tests()),
                /*restriction_product*/ None,
            ),
            allowed: allowed
                .iter()
                .map(|package| (*package).to_owned())
                .collect(),
        }
    }
}

impl SkillProvider for FixtureExecutorSkills {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            let mut catalog = self.inner.list(query).await?;
            catalog
                .entries
                .retain(|entry| self.allowed.contains(&entry.id.0));
            assert!(
                catalog
                    .entries
                    .iter()
                    .all(|entry| self.allowed.contains(&entry.id.0)),
                "ambient host packages must not enter the fixture catalog"
            );
            Ok(catalog)
        })
    }

    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        assert!(
            self.allowed.contains(&request.package.0),
            "the fixture must declare every executor installation it reads"
        );
        self.inner.read(request)
    }

    fn search(&self, request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult> {
        assert!(
            self.allowed.contains(&request.package.0),
            "the fixture must declare every executor installation it searches"
        );
        self.inner.search(request)
    }
}
