macro_rules! taxonomy {
    ($(#[$attribute:meta])* $name:ident { $($(#[$variant_attribute:meta])* $variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$attribute])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
        pub(crate) enum $name {
            $($(#[$variant_attribute])* #[serde(rename = $value)] $variant),+
        }

        impl $name {
            pub(crate) const VALUES: &'static [&'static str] = &[$($value),+];

            pub(super) fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }
    };
}

taxonomy!(Phase {
    Planning => "planning",
    Implementation => "implementation",
    Testing => "testing",
    Deployment => "deployment",
    Reporting => "reporting",
});

taxonomy!(Activity {
    Requirements => "requirements",
    Specification => "specification",
    RepositoryAnalysis => "repository_analysis",
    Research => "research",
    Diagnosis => "diagnosis",
    ArchitectureDesign => "architecture_design",
    WorkPlanning => "work_planning",
    Coding => "coding",
    Configuration => "configuration",
    Refactoring => "refactoring",
    DependencyOrBuildChange => "dependency_or_build_change",
    TestAuthoring => "test_authoring",
    DocumentationAuthoring => "documentation_authoring",
    DataOrSchemaChange => "data_or_schema_change",
    BuildValidation => "build_validation",
    UnitTesting => "unit_testing",
    IntegrationTesting => "integration_testing",
    BrowserQa => "browser_qa",
    CompatibilityTesting => "compatibility_testing",
    MigrationRehearsal => "migration_rehearsal",
    VerificationReview => "verification_review",
    Packaging => "packaging",
    Deployment => "deployment",
    Rollback => "rollback",
    RuntimeOperations => "runtime_operations",
    Monitoring => "monitoring",
    UserElaboration => "user_elaboration",
    StatusUpdate => "status_update",
    CompletionHandoff => "completion_handoff",
    ReviewFeedback => "review_feedback",
    Coordination => "coordination",
});

taxonomy!(#[derive(Default)] Relation {
    #[default]
    NewWork => "new_work",
    ReworkPrevious => "rework_previous",
});
