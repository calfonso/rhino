pub mod proto {
    pub mod mvccpb {
        tonic::include_proto!("mvccpb");
    }
    pub mod authpb {
        tonic::include_proto!("authpb");
    }
    pub mod etcdserverpb {
        tonic::include_proto!("etcdserverpb");
    }
}

pub mod backend;
pub mod drivers;
pub mod server;

pub use backend::{Backend, BackendError, Event, KeyValue, WatchResult};
pub use drivers::mysql::{MysqlBackend, MysqlConfig};
pub use drivers::postgres::{PostgresBackend, PostgresConfig};
pub use drivers::sqlite::{SqliteBackend, SqliteConfig};
pub use server::RhinoServer;
