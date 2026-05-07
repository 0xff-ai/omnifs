use omnifs_sdk::Cx;
use omnifs_sdk::http::Request;

use crate::State;

pub(crate) trait ArxivHttpExt {
    fn arxiv_get(&self, url: impl Into<String>) -> Request<'_, State>;
}

impl ArxivHttpExt for Cx<State> {
    fn arxiv_get(&self, url: impl Into<String>) -> Request<'_, State> {
        self.http()
            .get(url)
            .header("User-Agent", "omnifs-provider-arxiv/0.1.0")
    }
}
