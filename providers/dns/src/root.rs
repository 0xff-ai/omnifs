use std::net::IpAddr;

use omnifs_sdk::prelude::*;

use crate::query::read_reverse_bytes;
use crate::types::ResolverName;
use crate::{Result, State};

const DYNAMIC_CURSOR: &str = "dynamic";

fn mark_dynamic(projection: &mut Projection) {
    projection.page(PageStatus::More(Cursor::Opaque(DYNAMIC_CURSOR.to_string())));
}

pub struct RootHandlers;

#[handlers]
impl RootHandlers {
    #[dir("/")]
    fn root(cx: &DirCx<State>) -> Result<Projection> {
        let mut projection = Projection::new();
        for resolver in cx.state(|state| {
            state
                .resolvers
                .resolver_names()
                .into_iter()
                .map(|name| {
                    name.parse::<ResolverName>()
                        .map(|resolver| format!("@{resolver}"))
                        .map_err(|()| {
                            ProviderError::internal(format!(
                                "configured resolver name is invalid: {name}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>>>()
        })? {
            projection.dir(resolver);
        }
        mark_dynamic(&mut projection);
        Ok(projection)
    }

    #[file("/resolvers")]
    fn resolvers_file(cx: &Cx<State>) -> Result<FileContent> {
        let body = cx.state(|state| state.resolvers.format_resolvers_file().into_bytes());
        Ok(FileContent::bytes(body))
    }

    #[dir("/reverse")]
    fn reverse_dir() -> Result<Projection> {
        let mut projection = Projection::new();
        mark_dynamic(&mut projection);
        Ok(projection)
    }

    #[file("/reverse/{ip}")]
    async fn reverse_ip(cx: &Cx<State>, ip: IpAddr) -> Result<FileContent> {
        let bytes = read_reverse_bytes(cx, None, &ip.to_string()).await?;
        Ok(FileContent::bytes(bytes))
    }

    #[dir("/@{resolver}")]
    fn resolver_root(_resolver: ResolverName) -> Result<Projection> {
        let mut projection = Projection::new();
        mark_dynamic(&mut projection);
        Ok(projection)
    }

    #[dir("/@{resolver}/reverse")]
    fn resolver_reverse_dir(_resolver: ResolverName) -> Result<Projection> {
        let mut projection = Projection::new();
        mark_dynamic(&mut projection);
        Ok(projection)
    }

    #[file("/@{resolver}/reverse/{ip}")]
    async fn resolver_reverse_ip(
        cx: &Cx<State>,
        resolver: ResolverName,
        ip: IpAddr,
    ) -> Result<FileContent> {
        let bytes = read_reverse_bytes(cx, Some(&resolver), &ip.to_string()).await?;
        Ok(FileContent::bytes(bytes))
    }
}
