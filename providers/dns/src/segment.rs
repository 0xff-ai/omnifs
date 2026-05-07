use omnifs_sdk::prelude::*;

use crate::query::{read_record_bytes, record_names};
use crate::types::{DomainName, ResolverName};
use crate::{Result, State};

fn record_projection() -> Projection {
    let mut projection = Projection::new();
    for record in record_names() {
        projection.file(record);
    }
    projection.page(PageStatus::Exhaustive);
    projection
}

pub struct SegmentHandlers;

#[handlers]
impl SegmentHandlers {
    #[dir("/{domain}")]
    fn domain_dir(_domain: DomainName) -> Result<Projection> {
        Ok(record_projection())
    }

    #[dir("/@{resolver}/{domain}")]
    fn resolver_domain_dir(_resolver: ResolverName, _domain: DomainName) -> Result<Projection> {
        Ok(record_projection())
    }

    #[file("/{domain}/{record}")]
    async fn domain_record(
        cx: &Cx<State>,
        domain: DomainName,
        record: String,
    ) -> Result<FileContent> {
        let bytes = read_record_bytes(cx, None, &domain, &record).await?;
        Ok(FileContent::bytes(bytes))
    }

    #[file("/@{resolver}/{domain}/{record}")]
    async fn resolver_domain_record(
        cx: &Cx<State>,
        resolver: ResolverName,
        domain: DomainName,
        record: String,
    ) -> Result<FileContent> {
        let bytes = read_record_bytes(cx, Some(&resolver), &domain, &record).await?;
        Ok(FileContent::bytes(bytes))
    }
}
