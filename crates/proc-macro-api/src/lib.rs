//! Client-side Proc-Macro crate
//!
//! We separate proc-macro expanding logic to an extern program to allow
//! different implementations (e.g. wasm or dylib loading). And this crate
//! is used to provide basic infrastructure for communication between two
//! processes: Client (RA itself), Server (the external program)

pub mod legacy_protocol {
    pub mod json;
    pub mod msg;
}
mod process;

use paths::{AbsPath, AbsPathBuf};
use span::Span;
use std::{fmt, io, sync::Arc, time::SystemTime};

use crate::{
    legacy_protocol::msg::{
        ExpandMacro, ExpandMacroData, ExpnGlobals, FlatTree, HAS_GLOBAL_SPANS, PanicMessage,
        RUST_ANALYZER_SPAN_SUPPORT, Request, Response, SpanDataIndexMap,
        deserialize_span_data_index_map, flat::serialize_span_data_index_map,
    },
    process::ProcMacroServerProcess,
};

#[derive(Copy, Clone, Eq, PartialEq, Debug, serde_derive::Serialize, serde_derive::Deserialize)]
pub enum ProcMacroKind {
    CustomDerive,
    Attr,
    // This used to be called FuncLike, so that's what the server expects currently.
    #[serde(alias = "Bang")]
    #[serde(rename(serialize = "FuncLike", deserialize = "FuncLike"))]
    Bang,
}

/// A handle to an external process which load dylibs with macros (.so or .dll)
/// and runs actual macro expansion functions.
#[derive(Debug)]
pub struct ProcMacroClient {
    /// Currently, the proc macro process expands all procedural macros sequentially.
    ///
    /// That means that concurrent salsa requests may block each other when expanding proc macros,
    /// which is unfortunate, but simple and good enough for the time being.
    process: Arc<ProcMacroServerProcess>,
    path: AbsPathBuf,
}

pub struct MacroDylib {
    path: AbsPathBuf,
}

impl MacroDylib {
    pub fn new(path: AbsPathBuf) -> MacroDylib {
        MacroDylib { path }
    }
}

/// A handle to a specific proc-macro (a `#[proc_macro]` annotated function).
///
/// It exists within the context of a specific proc-macro server -- currently
/// we share a single expander process for all macros within a workspace.
#[derive(Debug, Clone)]
pub struct ProcMacro {
    process: Arc<ProcMacroServerProcess>,
    dylib_path: Arc<AbsPathBuf>,
    name: Box<str>,
    kind: ProcMacroKind,
    dylib_last_modified: Option<SystemTime>,
}

impl Eq for ProcMacro {}
impl PartialEq for ProcMacro {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.kind == other.kind
            && self.dylib_path == other.dylib_path
            && self.dylib_last_modified == other.dylib_last_modified
            && Arc::ptr_eq(&self.process, &other.process)
    }
}

#[derive(Clone, Debug)]
pub struct ServerError {
    pub message: String,
    pub io: Option<Arc<io::Error>>,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)?;
        if let Some(io) = &self.io {
            f.write_str(": ")?;
            io.fmt(f)?;
        }
        Ok(())
    }
}

impl ProcMacroClient {
    /// Spawns an external process as the proc macro server and returns a client connected to it.
    pub fn spawn(
        process_path: &AbsPath,
        env: impl IntoIterator<Item = (impl AsRef<std::ffi::OsStr>, impl AsRef<std::ffi::OsStr>)>
        + Clone,
    ) -> io::Result<ProcMacroClient> {
        let process = ProcMacroServerProcess::run(process_path, env)?;
        Ok(ProcMacroClient { process: Arc::new(process), path: process_path.to_owned() })
    }

    pub fn server_path(&self) -> &AbsPath {
        &self.path
    }

    /// Loads a proc-macro dylib into the server process returning a list of `ProcMacro`s loaded.
    pub fn load_dylib(&self, dylib: MacroDylib) -> Result<Vec<ProcMacro>, ServerError> {
        let _p = tracing::info_span!("ProcMacroServer::load_dylib").entered();
        let macros = self.process.find_proc_macros(&dylib.path)?;

        let dylib_path = Arc::new(dylib.path);
        let dylib_last_modified = std::fs::metadata(dylib_path.as_path())
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        match macros {
            Ok(macros) => Ok(macros
                .into_iter()
                .map(|(name, kind)| ProcMacro {
                    process: self.process.clone(),
                    name: name.into(),
                    kind,
                    dylib_path: dylib_path.clone(),
                    dylib_last_modified,
                })
                .collect()),
            Err(message) => Err(ServerError { message, io: None }),
        }
    }

    pub fn exited(&self) -> Option<&ServerError> {
        self.process.exited()
    }
}

impl ProcMacro {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> ProcMacroKind {
        self.kind
    }

    pub fn expand(
        &self,
        subtree: tt::SubtreeView<'_, Span>,
        attr: Option<tt::SubtreeView<'_, Span>>,
        env: Vec<(String, String)>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: Option<String>,
    ) -> Result<Result<tt::TopSubtree<Span>, PanicMessage>, ServerError> {
        let version = self.process.version();

        let mut span_data_table = SpanDataIndexMap::default();
        let def_site = span_data_table.insert_full(def_site).0;
        let call_site = span_data_table.insert_full(call_site).0;
        let mixed_site = span_data_table.insert_full(mixed_site).0;
        let task = ExpandMacro {
            data: ExpandMacroData {
                macro_body: FlatTree::new(subtree, version, &mut span_data_table),
                macro_name: self.name.to_string(),
                attributes: attr
                    .map(|subtree| FlatTree::new(subtree, version, &mut span_data_table)),
                has_global_spans: ExpnGlobals {
                    serialize: version >= HAS_GLOBAL_SPANS,
                    def_site,
                    call_site,
                    mixed_site,
                },
                span_data_table: if version >= RUST_ANALYZER_SPAN_SUPPORT {
                    serialize_span_data_index_map(&span_data_table)
                } else {
                    Vec::new()
                },
            },
            lib: self.dylib_path.to_path_buf().into(),
            env,
            current_dir,
        };

        let response = self.process.send_task(Request::ExpandMacro(Box::new(task)))?;

        match response {
            Response::ExpandMacro(it) => {
                Ok(it.map(|tree| FlatTree::to_subtree_resolved(tree, version, &span_data_table)))
            }
            Response::ExpandMacroExtended(it) => Ok(it.map(|resp| {
                FlatTree::to_subtree_resolved(
                    resp.tree,
                    version,
                    &deserialize_span_data_index_map(&resp.span_data_table),
                )
            })),
            _ => Err(ServerError { message: "unexpected response".to_owned(), io: None }),
        }
    }
}
