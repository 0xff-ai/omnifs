//! `omnifs logs` streams daemon-owned log bytes over the control socket.

use bytes::Bytes;
use clap::Args;
use omnifs_api::grpc::wire::log_stream_item::Value;

use crate::rpc::RpcClient;
use crate::ui::output::Output;

#[derive(Args, Debug, Clone, Default)]
pub struct LogsArgs {
    #[arg(short = 'f', long)]
    pub follow: bool,
    #[arg(long, default_value_t = 50)]
    pub lines: u32,
}

fn log_data(item: omnifs_api::grpc::wire::LogStreamItem) -> anyhow::Result<Bytes> {
    match item.value {
        Some(Value::Data(data)) => Ok(data),
        Some(Value::Ready(_)) => {
            anyhow::bail!("daemon returned a repeated log stream Ready item")
        },
        None => anyhow::bail!("daemon returned an empty log stream item"),
    }
}

impl LogsArgs {
    pub async fn run(self, output: &Output) -> anyhow::Result<()> {
        if output.is_structured() {
            anyhow::bail!("logs is a passthrough command and only supports human output")
        }
        let mut stream = RpcClient::resolve()?
            .stream_logs(self.follow, self.lines)
            .await?;
        output.narrate(if self.follow {
            "streaming daemon logs (Ctrl-C to stop)"
        } else {
            "showing daemon logs"
        });
        while let Some(item) = stream.message().await? {
            output.write_raw_bytes(&log_data(item)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_stream_rejects_items_after_ready() {
        let repeated_ready = omnifs_api::grpc::wire::LogStreamItem {
            value: Some(Value::Ready(omnifs_api::grpc::wire::LogsReady {
                instance_id: "same".into(),
            })),
        };
        let empty = omnifs_api::grpc::wire::LogStreamItem { value: None };
        assert!(log_data(repeated_ready).is_err());
        assert!(log_data(empty).is_err());
    }

    #[test]
    fn log_stream_preserves_raw_data_bytes() {
        let data = Bytes::from_static(b"\0raw\xff");
        let item = omnifs_api::grpc::wire::LogStreamItem {
            value: Some(Value::Data(data.clone())),
        };
        assert_eq!(log_data(item).expect("data item"), data);
    }
}
