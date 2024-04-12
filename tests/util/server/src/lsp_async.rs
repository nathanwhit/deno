// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use crate::deno_exe_path;
use crate::jsr_registry_url;
use crate::npm_registry_url;
use crate::PathRef;

use super::TempDir;

use anyhow::Result;
use lsp_types as lsp;
use lsp_types::ClientCapabilities;
use lsp_types::ClientInfo;
use lsp_types::CodeActionCapabilityResolveSupport;
use lsp_types::CodeActionClientCapabilities;
use lsp_types::CodeActionKindLiteralSupport;
use lsp_types::CodeActionLiteralSupport;
use lsp_types::CompletionClientCapabilities;
use lsp_types::CompletionItemCapability;
use lsp_types::FoldingRangeClientCapabilities;
use lsp_types::InitializeParams;
use lsp_types::TextDocumentClientCapabilities;
use lsp_types::TextDocumentSyncClientCapabilities;
use lsp_types::Url;
use lsp_types::WorkspaceClientCapabilities;
use once_cell::sync::Lazy;
use parking_lot::Condvar;
use parking_lot::Mutex;
use regex::Regex;
use serde::de;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::to_value;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

async fn client_task() {
    
}