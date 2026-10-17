use super::{
    PrivateUsagePolicy, ProviderUsageAdapter, ProviderUsageContribution, ProviderUsageError,
    ProviderUsageRequest,
};

pub(super) struct PiUsageAdapter;

pub(super) static ADAPTER: PiUsageAdapter = PiUsageAdapter;

impl ProviderUsageAdapter for PiUsageAdapter {
    fn family_id(&self) -> &'static str {
        crate::agent_registry::PI_AGENT_ID
    }

    fn private_usage_policy(&self) -> PrivateUsagePolicy {
        // Pi declares no private usage schema; standard ACP usage is the
        // only enabled source.
        PrivateUsagePolicy::StandardAcpOnly
    }

    fn trusted_reporter_ids(&self) -> &'static [&'static str] {
        &[]
    }

    fn extract_private_usage(
        &self,
        request: ProviderUsageRequest<'_>,
    ) -> Result<ProviderUsageContribution, ProviderUsageError> {
        super::no_verified_private_usage(request)
    }
}
