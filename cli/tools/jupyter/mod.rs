// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

// TODO(bartlomieju): remove me
#![allow(unused)]

use crate::create_main_worker;
use crate::flags::Flags;
use crate::flags::JupyterFlags;
use crate::proc_state::ProcState;
use crate::tools::repl::EvaluationOutput;
use crate::tools::repl::ReplSession;
use data_encoding::HEXLOWER;
use deno_core::anyhow::anyhow;
use deno_core::anyhow::Context;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::futures::channel::mpsc;
use deno_core::futures::channel::mpsc::UnboundedReceiver;
use deno_core::futures::channel::mpsc::UnboundedSender;
use deno_core::futures::SinkExt;
use deno_core::futures::StreamExt;
use deno_core::op_sync;
use deno_core::resolve_url_or_path;
use deno_core::serde::Deserialize;
use deno_core::serde::Serialize;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::JsRuntime;
use deno_core::OpState;
use deno_runtime::permissions::Permissions;
use deno_runtime::worker::MainWorker;
use ring::hmac;
use std::collections::HashMap;
use std::convert::From;
use std::env::current_exe;
use std::time::Duration;
use tempfile::TempDir;
use tokio::join;
use tokio::time::sleep;
use zeromq::prelude::*;
use zeromq::ZmqMessage;

mod comm;
mod install;
mod message_types;

use comm::DealerComm;
use comm::HbComm;
use comm::PubComm;
pub use install::install;
use message_types::*;

pub async fn kernel(
  flags: Flags,
  jupyter_flags: JupyterFlags,
) -> Result<(), AnyError> {
  if jupyter_flags.conn_file.is_none() {
    return Err(generic_error("Missing --conn flag"));
  }

  let conn_file_path = jupyter_flags.conn_file.unwrap();

  let mut kernel = Kernel::new(flags, conn_file_path.to_str().unwrap()).await?;
  println!("[DENO] kernel created: {:#?}", kernel.session_id);

  println!("running kernel...");
  kernel.run().await;
  println!("done running kernel.");

  Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum KernelState {
  Busy,
  Idle,
  Starting,
}

struct Kernel {
  metadata: KernelMetadata,
  conn_spec: ConnectionSpec,
  state: KernelState,
  iopub_comm: PubComm,
  shell_comm: DealerComm,
  control_comm: DealerComm,
  stdin_comm: DealerComm,
  hb_comm: HbComm,
  session_id: String,
  execution_count: u32,
  repl_session: ReplSession,
  stdio_rx: StdioProxyReceiver,
  last_comm_ctx: Option<CommContext>,
}

#[derive(Copy, Clone, Debug)]
enum HandlerType {
  Shell,
  Control,
  Stdin,
}

type StdioProxySender = UnboundedSender<(StdioType, String)>;
type StdioProxyReceiver = UnboundedReceiver<(StdioType, String)>;

fn init(rt: &mut JsRuntime) {
  rt.overwrite_op("op_print", op_sync(op_print));
}

pub fn op_print(
  state: &mut OpState,
  msg: String,
  is_err: bool,
) -> Result<(), AnyError> {
  let mut sender = state.borrow_mut::<StdioProxySender>();

  if is_err {
    let r = sender.unbounded_send((StdioType::Stderr, msg));
  } else {
    let r = sender.unbounded_send((StdioType::Stdout, msg));
  }
  Ok(())
}

impl Kernel {
  async fn new(flags: Flags, conn_file_path: &str) -> Result<Self, AnyError> {
    let conn_file =
      std::fs::read_to_string(conn_file_path).with_context(|| {
        format!("Couldn't read connection file: {}", conn_file_path)
      })?;
    let spec: ConnectionSpec =
      serde_json::from_str(&conn_file).with_context(|| {
        format!("Connection file isn't proper JSON: {}", conn_file_path)
      })?;

    println!("[DENO] parsed conn file: {:#?}", spec);

    let execution_count = 0;
    let session_id = uuid::Uuid::new_v4().to_string();
    let hmac_key = hmac::Key::new(hmac::HMAC_SHA256, spec.key.as_ref());
    let metadata = KernelMetadata::default();
    let iopub_comm = PubComm::new(
      create_conn_str(&spec.transport, &spec.ip, spec.iopub_port),
      session_id.clone(),
      hmac_key.clone(),
    );
    let shell_comm = DealerComm::new(
      "shell",
      create_conn_str(&spec.transport, &spec.ip, spec.shell_port),
      session_id.clone(),
      hmac_key.clone(),
    );
    let control_comm = DealerComm::new(
      "control",
      create_conn_str(&spec.transport, &spec.ip, spec.control_port),
      session_id.clone(),
      hmac_key.clone(),
    );
    let stdin_comm = DealerComm::new(
      "stdin",
      create_conn_str(&spec.transport, &spec.ip, spec.stdin_port),
      session_id.clone(),
      hmac_key,
    );
    let hb_comm =
      HbComm::new(create_conn_str(&spec.transport, &spec.ip, spec.hb_port));

    let main_module = resolve_url_or_path("./$deno$jupyter.ts").unwrap();
    // TODO(bartlomieju): should we run with all permissions?
    let permissions = Permissions::allow_all();
    let ps = ProcState::build(flags.clone()).await?;
    let mut worker =
      create_main_worker(&ps, main_module.clone(), permissions, Some(&init));
    let (stdio_tx, stdio_rx) = mpsc::unbounded();
    worker
      .js_runtime
      .op_state()
      .borrow_mut()
      .put::<StdioProxySender>(stdio_tx);
    let repl_session = ReplSession::initialize(worker).await?;

    let kernel = Self {
      metadata,
      conn_spec: spec,
      state: KernelState::Idle,
      iopub_comm,
      shell_comm,
      control_comm,
      stdin_comm,
      hb_comm,
      session_id,
      execution_count,
      repl_session,
      stdio_rx,
      last_comm_ctx: None,
    };

    Ok(kernel)
  }

  async fn run(&mut self) -> Result<(), AnyError> {
    println!("Connecting to iopub");
    self.iopub_comm.connect().await?;
    println!("Connected to iopub");
    println!("Connecting to shell");
    self.shell_comm.connect().await?;
    println!("Connected to shell");
    println!("Connecting to control");
    self.control_comm.connect().await?;
    println!("Connected to control");
    println!("Connecting to stdin");
    self.stdin_comm.connect().await?;
    println!("Connected to stdin");
    println!("Connecting to heartbeat");
    self.hb_comm.connect().await?;
    println!("Connected to heartbeat");

    let mut poll_worker = true;
    loop {
      tokio::select! {
        shell_msg_result = self.shell_comm.recv() => {
          self.handler(HandlerType::Shell, shell_msg_result).await;
          poll_worker = true;
        },
        control_msg_result = self.control_comm.recv() => {
          self.handler(HandlerType::Control, control_msg_result).await;
          poll_worker = true;
        },
        stdin_msg_result = self.stdin_comm.recv() => {
          self.handler(HandlerType::Stdin, stdin_msg_result).await;
          poll_worker = true;
        },
        maybe_stdio_proxy_msg = self.stdio_rx.next() => {
          if let Some(stdio_proxy_msg) = maybe_stdio_proxy_msg {
            self.stdio_handler(stdio_proxy_msg).await;
          }
        },
        heartbeat_result = self.hb_comm.heartbeat() => {
          if let Err(e) = heartbeat_result {
            println!("[heartbeat] error: {}", e);
          }
        },
        _ = self.repl_session.run_event_loop(), if poll_worker => {
          poll_worker = false;
        }
      }
    }
  }

  async fn handler(
    &mut self,
    handler_type: HandlerType,
    recv_result: Result<RequestMessage, AnyError>,
  ) {
    let req_msg = match recv_result {
      Ok(m) => m,
      Err(e) => {
        println!("error receiving msg: {}", e);
        return;
      }
    };

    let comm_ctx = CommContext {
      session_id: self.session_id.clone(),
      message: req_msg,
    };
    self.last_comm_ctx = Some(comm_ctx.clone());

    println!("[DENO] set_state busy {:#?}", handler_type);
    self.set_state(&comm_ctx, KernelState::Busy).await;

    let major_version = &comm_ctx.message.header.version.to_string()[0..1];
    let res = match (handler_type, major_version) {
      // TODO(apowers313) implement new and old Jupyter protocols here
      (HandlerType::Shell, "5") => self.shell_handler(&comm_ctx).await,
      (HandlerType::Control, "5") => self.control_handler(&comm_ctx),
      (HandlerType::Stdin, "5") => self.stdin_handler(&comm_ctx),
      _ => Err(anyhow!(
        "No handler for message: '{}' v{}",
        comm_ctx.message.header.msg_type,
        major_version
      )),
    };

    match res {
      Ok(_) => {}
      Err(e) => {
        println!(
          "Error handling packet '{}': {}",
          comm_ctx.message.header.msg_type, e
        );
      }
    };

    println!("[DENO] set_state idle {:#?}", handler_type);
    self.set_state(&comm_ctx, KernelState::Idle).await;
  }

  async fn stdio_handler(
    &mut self,
    stdio_proxy_msg: (StdioType, String),
  ) -> Result<(), AnyError> {
    if let Some(comm_ctx) = self.last_comm_ctx.as_ref() {
      let (t, content) = stdio_proxy_msg;
      let msg = SideEffectMessage::new(
        comm_ctx,
        "stream",
        ReplyMetadata::Empty,
        ReplyContent::Stream(StreamContent {
          name: match t {
            StdioType::Stdout => "stdout".to_string(),
            StdioType::Stderr => "stderr".to_string(),
          },
          text: content,
        }),
      );
      self.iopub_comm.send(msg).await?;
    } else {
      println!(
        "Received stdio message, but there is no last CommContext: {:#?}",
        stdio_proxy_msg.1
      );
    }

    Ok(())
  }

  async fn shell_handler(
    &mut self,
    comm_ctx: &CommContext,
  ) -> Result<(), AnyError> {
    let msg_type = comm_ctx.message.header.msg_type.as_ref();
    let result = match msg_type {
      "kernel_info_request" => self.kernel_info_reply(comm_ctx).await,
      "execute_request" => self.execute_request(comm_ctx).await,
      _ => {
        println!("[shell] no handler for {}", msg_type);
        Ok(())
      }
    };

    if let Err(e) = result {
      println!("[shell] error handling {}: {}", msg_type, e);
    } else {
      println!("[shell] ok {}", msg_type);
    }

    Ok(())
  }

  fn control_handler(&self, comm_ctx: &CommContext) -> Result<(), AnyError> {
    todo!()
  }

  fn stdin_handler(&self, comm_ctx: &CommContext) -> Result<(), AnyError> {
    todo!()
  }

  async fn set_state(&mut self, comm_ctx: &CommContext, state: KernelState) {
    if self.state == state {
      println!("[DENO] set_state sets the same state: {:#?}", state);
      return;
    }

    self.state = state;

    let s = match state {
      KernelState::Busy => "busy",
      KernelState::Idle => "idle",
      KernelState::Starting => "starting",
    };

    let msg = SideEffectMessage::new(
      comm_ctx,
      "status",
      ReplyMetadata::Empty,
      ReplyContent::Status(KernelStatusContent {
        execution_state: s.to_string(),
      }),
    );

    if let Err(e) = self.iopub_comm.send(msg).await {
      println!("[IoPub] Error setting state: {}", s);
    }
  }

  async fn kernel_info_reply(
    &mut self,
    comm_ctx: &CommContext,
  ) -> Result<(), AnyError> {
    let content = KernelInfoReplyContent {
      status: String::from("ok"),
      protocol_version: self.metadata.protocol_version.clone(),
      implementation_version: self.metadata.kernel_version.clone(),
      implementation: self.metadata.implementation_name.clone(),
      language_info: KernelLanguageInfo {
        name: self.metadata.language.clone(),
        version: self.metadata.language_version.clone(),
        mimetype: self.metadata.mime.clone(),
        file_extension: self.metadata.file_ext.clone(),
      },
      help_links: vec![], // TODO(apowers313) dig up help links
      banner: self.metadata.banner.clone(),
      debugger: false,
    };

    let reply = ReplyMessage::new(
      comm_ctx,
      "kernel_info_reply",
      ReplyMetadata::Empty,
      ReplyContent::KernelInfo(content),
    );

    self.shell_comm.send(reply).await?;

    Ok(())
  }

  async fn execute_request(
    &mut self,
    comm_ctx: &CommContext,
  ) -> Result<(), AnyError> {
    self.execution_count += 1;

    let exec_request_content = match &comm_ctx.message.content {
      RequestContent::Execute(c) => c,
      _ => return Err(anyhow!("malformed execution content")),
    };

    let input_msg = SideEffectMessage::new(
      comm_ctx,
      "execute_input",
      ReplyMetadata::Empty,
      ReplyContent::ExecuteInput(ExecuteInputContent {
        code: exec_request_content.code.clone(),
        execution_count: self.execution_count,
      }),
    );
    self.iopub_comm.send(input_msg).await?;

    // let output = self
    //   .repl_session
    //   .evaluate_line_and_get_output(&exec_request_content.code)
    //   .await?;

    //     let test_svg = r###"<?xml version="1.0" encoding="iso-8859-1"?>
    // <svg version="1.1" id="Capa_1" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" x="0px" y="0px"
    // 	 viewBox="0 0 299.429 299.429" style="enable-background:new 0 0 299.429 299.429;" xml:space="preserve">
    // <g>
    // 	<path style="fill:#010002;" d="M245.185,44.209H54.245L0,116.533l149.715,138.688l149.715-138.682L245.185,44.209z
    // 		 M206.746,121.778l-57.007,112.1l-56.53-112.1H206.746z M98.483,109.844l51.232-51.232l51.232,51.232H98.483z M164.119,56.142
    // 		h69.323L213.876,105.9L164.119,56.142z M86.311,105.142l-16.331-49h65.331L86.311,105.142z M79.849,121.778l49.632,98.429
    // 		L23.223,121.778H79.849z M220.136,121.778h56.071l-106.013,98.203L220.136,121.778z M225.148,109.844l18.694-47.538l35.652,47.538
    // 		H225.148z M58.266,58.738l17.035,51.112H19.929L58.266,58.738z"/>
    // </g>
    // </svg>
    // "###.to_string();
    //     let mut dd = DisplayData::new();
    //     dd.add("image/svg+xml", test_svg);
    //     let dd_msg = SideEffectMessage::new(
    //       comm_ctx,
    //       "display_data",
    //       ReplyMetadata::Empty,
    //       ReplyContent::DisplayData(DisplayDataContent {
    //         data: dd.to_value()?,
    //         metadata: json!({}),
    //         transient: json!({}),
    //       }),
    //     );
    //     self.iopub_comm.send(dd_msg).await?;

    let output = self
      .repl_session
      .evaluate_line_with_object_wrapping(&exec_request_content.code)
      .await?;

    let result = if output.value["exceptionDetails"].is_object() {
      let stack_trace: Vec<String> = output.value["exceptionDetails"]
        ["exception"]["description"]
        .as_str()
        .unwrap()
        .split("\n")
        .map(|s| s.to_string())
        .collect();
      dbg!(&stack_trace);
      println!("err_value: {}", stack_trace.first().unwrap().to_string());
      ExecResult::Error(ExecError {
        // TODO(apowers313) this could probably use smarter unwrapping -- for example, someone may throw non-object
        err_name: output.value["exceptionDetails"]["exception"]["className"]
          .to_string(),
        err_value: stack_trace.first().unwrap().to_string(),
        // output.value["exceptionDetails"]["stackTrace"]["callFrames"]
        stack_trace,
      })
    } else {
      ExecResult::Ok(output.value["result"].clone())
    };

    match result {
      ExecResult::Ok(_) => {
        self.send_execute_reply_ok(comm_ctx).await?;
        self.send_execute_result(comm_ctx, &result).await?;
      }
      ExecResult::Error(_) => {
        self.send_execute_reply_error(comm_ctx, &result).await?;
        self.send_error(comm_ctx, &result).await?;
      }
    };

    Ok(())
  }

  async fn send_execute_reply_ok(
    &mut self,
    comm_ctx: &CommContext,
  ) -> Result<(), AnyError> {
    println!("sending exec result");
    let msg = ReplyMessage::new(
      comm_ctx,
      "execute_reply",
      ReplyMetadata::Empty,
      ReplyContent::ExecuteReply(ExecuteReplyContent {
        status: "ok".to_string(),
        execution_count: self.execution_count,
        // NOTE(bartlomieju): these two fields are always empty
        payload: vec![],
        user_expressions: json!({}),
      }),
    );
    self.shell_comm.send(msg).await?;

    Ok(())
  }

  async fn send_execute_reply_error(
    &mut self,
    comm_ctx: &CommContext,
    result: &ExecResult,
  ) -> Result<(), AnyError> {
    let e = match result {
      ExecResult::Error(e) => e,
      _ => return Err(anyhow!("send_execute_reply_error: unreachable")),
    };
    let msg = ReplyMessage::new(
      comm_ctx,
      "execute_reply",
      ReplyMetadata::Empty,
      ReplyContent::ExecuteError(ExecuteErrorContent {
        status: "error".to_string(),
        payload: vec![],
        user_expressions: json!({}),
        // TODO(apowers313) implement error messages
        ename: e.err_name.clone(),
        evalue: e.err_value.clone(),
        traceback: e.stack_trace.clone(),
      }),
    );
    self.shell_comm.send(msg).await?;

    Ok(())
  }

  async fn send_error(
    &mut self,
    comm_ctx: &CommContext,
    result: &ExecResult,
  ) -> Result<(), AnyError> {
    let e = match result {
      ExecResult::Error(e) => e,
      _ => return Err(anyhow!("send_error: unreachable")),
    };
    let msg = SideEffectMessage::new(
      comm_ctx,
      "error",
      ReplyMetadata::Empty,
      ReplyContent::Error(ErrorContent {
        ename: e.err_name.clone(),
        evalue: e.err_value.clone(),
        traceback: e.stack_trace.clone(),
      }),
    );
    self.iopub_comm.send(msg).await?;

    Ok(())
  }

  async fn send_execute_result(
    &mut self,
    comm_ctx: &CommContext,
    result: &ExecResult,
  ) -> Result<(), AnyError> {
    let v = match result {
      ExecResult::Ok(v) => v,
      _ => return Err(anyhow!("send_execute_result: unreachable")),
    };
    let mut display_data = DisplayData::from(v.clone());
    if display_data.is_empty() {
      return Ok(());
    }

    let msg = SideEffectMessage::new(
      comm_ctx,
      "execute_result",
      ReplyMetadata::Empty,
      ReplyContent::ExecuteResult(ExecuteResultContent {
        execution_count: self.execution_count,
        data: display_data.to_object(),
        // data: json!("<not implemented>"),
        metadata: json!({}),
      }),
    );
    self.iopub_comm.send(msg).await?;

    Ok(())
  }

  async fn send_stdio(
    &mut self,
    comm_ctx: &CommContext,
    t: StdioType,
    text: &str,
  ) -> Result<(), AnyError> {
    let content = StreamContent {
      name: match t {
        StdioType::Stdout => "stdout".to_string(),
        StdioType::Stderr => "stderr".to_string(),
      },
      text: text.to_string(),
    };

    let msg = SideEffectMessage::new(
      comm_ctx,
      "stream",
      ReplyMetadata::Empty,
      ReplyContent::Stream(content),
    );

    self.iopub_comm.send(msg).await?;

    Ok(())
  }

  async fn send_display_data(
    &mut self,
    comm_ctx: &CommContext,
    display_data: DisplayData,
  ) -> Result<(), AnyError> {
    if (display_data.is_empty()) {
      return Err(anyhow!("send_display_data called with empty DisplayData"));
    }
    let data = display_data.to_object();

    let msg = SideEffectMessage::new(
      comm_ctx,
      "display_data",
      ReplyMetadata::Empty,
      ReplyContent::DisplayData(DisplayDataContent {
        data,
        metadata: json!({}),
        transient: json!({}),
      }),
    );
    self.iopub_comm.send(msg);

    Ok(())
  }
}

struct DisplayData {
  data_list: Vec<(String, Value)>,
}

impl DisplayData {
  fn new() -> DisplayData {
    Self { data_list: vec![] }
  }

  fn add(&mut self, mime_type: &str, data: Value) {
    self.data_list.push((mime_type.to_string(), data));
  }

  fn is_empty(&self) -> bool {
    if self.data_list.len() == 0 {
      return true;
    }

    return false;
  }

  fn to_object(&self) -> Value {
    let mut data = json!({});
    for d in self.data_list.iter() {
      data[&d.0] = json!(&d.1);
    }

    data
  }
}

impl From<Value> for DisplayData {
  fn from(data: Value) -> Self {
    dbg!(&data);
    let mut d = &data;
    let mut ret = DisplayData::new();
    // if we passed in a result, unwrap it
    d = if d["result"].is_object() {
      &d["result"]
    } else {
      d
    };

    if !d["type"].is_string() {
      // not an execution result
      return ret;
    }

    let mut t = match &d["type"] {
      Value::String(x) => x.to_string(),
      _ => return ret,
    };

    if t == "object".to_string()
      && d["subtype"] == Value::String("null".to_string())
    {
      // JavaScript null, the gift that keeps on giving
      t = "null".to_string();
    }

    match t.as_ref() {
      // TODO(apowers313) inspect object / call toPng, toHtml, toSvg, toText, toMime
      "object" => {
        // TODO: this description isn't very useful
        ret.add("text/plain", d["description"].clone());
        ret.add("application/json", d["description"].clone());
      }
      "string" => {
        ret.add("text/plain", d["value"].clone());
        ret.add("application/json", d["value"].clone());
      }
      "null" => {
        ret.add("text/plain", Value::String("null".to_string()));
        ret.add("application/json", Value::Null);
      }
      "bigint" => {
        // TODO: hmmm... is this really what we want?
        ret.add("text/plain", d["unserializableValue"].clone());
        ret.add("application/json", d["unserializableValue"].clone());
      }
      "symbol" => {
        ret.add("text/plain", d["description"].clone());
        ret.add("application/json", d["description"].clone());
      }
      "boolean" => {
        ret.add("text/plain", Value::String(d["value"].to_string()));
        ret.add("application/json", d["value"].clone());
      }
      "number" => {
        ret.add("text/plain", Value::String(d["value"].to_string()));
        ret.add("application/json", d["value"].clone());
      }
      // TODO(apowers313) currently ignoring "undefined" return value, I think most kernels make this a configuration option
      "undefined" => return ret,
      _ => {
        println!("unknown type in DisplayData::from: {}", t);
        return ret;
      }
    };

    ret
  }
}

// fn value_to_error(v: Value) -> Option(ErrorValue) {
//   if !v["exceptionDetails"].is_object() {
//     None
//   } else {
//     ErrorValue {
//       // v["exceptionDetails"]
//     }
//   }
// }

struct ErrorValue {}

enum ExecResult {
  Ok(Value),
  Error(ExecError),
}

struct ExecError {
  err_name: String,
  err_value: String,
  stack_trace: Vec<String>,
}

#[derive(Debug)]
struct KernelMetadata {
  banner: String,
  file_ext: String,
  help_text: String,
  help_url: String,
  implementation_name: String,
  kernel_version: String,
  language_version: String,
  language: String,
  mime: String,
  protocol_version: String,
}

impl Default for KernelMetadata {
  fn default() -> Self {
    Self {
      banner: "Welcome to Deno kernel".to_string(),
      file_ext: ".ts".to_string(),
      help_text: "<TODO>".to_string(),
      help_url: "https://github.com/denoland/deno".to_string(),
      implementation_name: "Deno kernel".to_string(),
      kernel_version: crate::version::deno(),
      language_version: crate::version::TYPESCRIPT.to_string(),
      language: "typescript".to_string(),
      mime: "text/x.typescript".to_string(),
      protocol_version: "5.3".to_string(),
    }
  }
}

#[derive(Debug, Deserialize)]
struct ConnectionSpec {
  ip: String,
  transport: String,
  control_port: u32,
  shell_port: u32,
  stdin_port: u32,
  hb_port: u32,
  iopub_port: u32,
  signature_scheme: String,
  key: String,
}

fn create_conn_str(transport: &str, ip: &str, port: u32) -> String {
  format!("{}://{}:{}", transport, ip, port)
}

#[derive(Debug)]
enum StdioType {
  Stdout,
  Stderr,
}