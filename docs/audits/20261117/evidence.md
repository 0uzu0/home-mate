# 隔离验证证据附录

本附录保存合成数据验证所用的小型源码与输出摘录。所有路径引用当时固定版本；将路径替换为对应 checkout 即可复用。直接链接真实构建库的 Rust 程序须使用同一工具链与依赖产物。它们是**缺陷行为复现**，不是修复后的验收测试，未连接设备、NAS 或真实第三方服务。

## 架构 SQL 输出

```json
{
  "migration_version_after_interrupt": 7,
  "migration_retry_error": "duplicate column name: pinned",
  "rows_after_reset_including_secrets_true": {
    "agent_md_revisions": 1,
    "user_md_entries": 1,
    "shared_profile_cache": 1,
    "shared_memory_outbox": 1
  }
}
```

## 仲裁复现源码

```rust
use std::sync::{Arc, Mutex, MutexGuard};
pub fn lock_or_recover<'a, T>(m: &'a Mutex<T>, _: &str) -> MutexGuard<'a, T> { m.lock().unwrap_or_else(|e|e.into_inner()) }
mod id { pub fn next(_: &str)->String { "audit-session".to_string() } }
mod store { pub struct Store; }
mod fleet {
  use std::sync::Arc;
  pub struct FleetService;
  pub struct Doc { pub nodes: Vec<()> }
  impl FleetService {
    pub fn new(_: Arc<crate::store::Store>)->Arc<Self> { Arc::new(Self) }
    pub fn doc(&self)->Doc { Doc { nodes: vec![] } }
    pub async fn wake_claim(&self, _: &serde_json::Value, _: std::time::Duration)->(usize,Vec<serde_json::Value>) { (0,vec![]) }
  }
}
#[path = "/path/to/home-mate-b/crates/hm-core/src/sovereignty.rs"]
mod sovereignty;
#[path = "/path/to/home-mate-b/crates/hm-core/src/arbitration.rs"]
mod arbitration;
fn claim(dev:&str,t:i64,db:f32,room:&str)->arbitration::Claim {
  arbitration::Claim::new(dev,t,Some(db)).with_room(room).with_snr_db(Some(20.0)).with_echo_health(Some("healthy".into())).with_profile_version(Some("v1".into())).with_calibrated(true)
}
fn main() {
  let a=claim("a",10_000,-20.0,"living"); let b=claim("b",11_000,-25.0,"bedroom"); let all=[a.clone(),b.clone()];
  println!("different_room_requests_1000ms_apart: a_winner={} b_winner={}", arbitration::decide_around(&a,&all,&[]).unwrap().winner.device,arbitration::decide_around(&b,&all,&[]).unwrap().winner.device);
  let pa=arbitration::Arbiter::new(Arc::new(store::Store));let pb=arbitration::Arbiter::new(Arc::new(store::Store));
  pa.record_passive_presence(claim("b",10_000,-20.0,"living"));pb.record_passive_presence(claim("c",10_000,-22.0,"bedroom"));
  let kws=claim("a",10_000,-35.0,"hallway");pa.inbound(kws.clone());pb.inbound(kws);
  println!("one_kws_two_passive_nodes: b_won={} c_won={}",pa.won_recently("b",std::time::Duration::from_secs(5)),pb.won_recently("c",std::time::Duration::from_secs(5)));
  let d=sovereignty::RoomSessionDomain::new();
  d.record_playback_notice(sovereignty::FleetPlaybackNotice{device:"a".into(),room:"living".into(),channel:"voice".into(),active:true,text_preview:"你好高达".into(),at_ms:10_000,ttl_ms:15_000});
  println!("real_near_end_wake_blocked_by_matching_peer_text={}",d.suppress_bleed_for_phrase("b","你好高达",false,true,11_000).is_some());
  println!("ongoing_playback_notice_live_after_15s={}",d.suppress_bleed_for_phrase("b","你好高达",false,false,25_001).is_some());
  let isolated=sovereignty::RoomSessionDomain::new();
  println!("other_core_owner_unseen_retreat={:?}",isolated.can_retreat("hallway","b",false,11_000));
}
```

## 仲裁复现 Cargo 配置

```toml
[package]
name = "hm-arbitration-repro"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["time"] }
```

## 交互复现源码

```rust
use std::{sync::Arc, time::Duration};
use hm_core::{store::Store,session::SessionStore,sovereignty::RoomSessionDomain};
fn main() {
 let store=Arc::new(Store::open_in_memory().unwrap());
 let sessions=SessionStore::new(store);
 let domain=RoomSessionDomain::new();
 let at=1_000;
 let owner=domain.claim_or_renew("room","dot2-test",at,9_000,240_000);
 sessions.ensure("dev:dot2-test").unwrap();
 sessions.append("dev:dot2-test","user","Old context marker").unwrap();
 assert!(domain.owner_for_room("room","dot2-test",at+240_001).is_none());
 let renewed=domain.claim_or_renew("room","dot2-test",at+240_001,9_000,240_000);
 assert_ne!(owner.session_id,renewed.session_id);
 let same=sessions.ensure("dev:dot2-test").unwrap();
 assert_eq!(sessions.history(&same,12).unwrap().len(),1);
 println!("PASS context reproduction: expired rsess replaced, old dev session history still replayed");
 let runtime=tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
 runtime.block_on(async {
   let (tx,rx)=tokio::sync::oneshot::channel::<()>();
   drop(tx);
   let result=tokio::time::timeout(Duration::from_millis(1500),rx).await;
   assert!(matches!(result,Ok(Err(_))));
   assert!(result.is_ok());
   println!("PASS stop receipt expression: dropped sender becomes confirmed=true with await.is_ok()");
 });
}
```

## 交互输出

```text
PASS context reproduction: expired rsess replaced, old dev session history still replayed
PASS stop receipt expression: dropped sender becomes confirmed=true with await.is_ok()
```

## 记忆与动态名称复现源码

```rust
use std::sync::{Arc, atomic::AtomicBool};
use hm_core::store::Store;
use hm_core::working_memory::{WorkingMemoryStore,ScopeContext};
use hm_core::tool::{Registry,ToolCtx};
use hm_core::tool::memory::{WorkingMemoryGetTool,WorkingMemorySetTool,WorkingMemoryDeleteTool};
use serde_json::json;
fn main() {
 let store=Arc::new(Store::open_in_memory().unwrap());
 let wm=WorkingMemoryStore::new(store);
 wm.set("alice","dot2-private","private_note","AUDIT_SYNTHETIC_ALICE_SECRET").unwrap();
 let caller=ScopeContext {device_id:Some("dot2-public".into()),room:None,person_id:None};
 assert!(wm.render_for_prompt_scoped(&caller).is_none());
 let mut reg=Registry::new();
 reg.register(Arc::new(WorkingMemoryGetTool::new(wm.clone())));
 reg.register(Arc::new(WorkingMemorySetTool::new(wm.clone())));
 reg.register(Arc::new(WorkingMemoryDeleteTool::new(wm.clone())));
 let ctx=ToolCtx::new("audit-task","audit-call",Arc::new(AtomicBool::new(false))).with_provenance(caller.device_id,Some("audit-trace".into()));
 let read=futures::executor::block_on(reg.call("workmem.get",json!({}),&ctx));
 assert!(read.result.unwrap().to_string().contains("AUDIT_SYNTHETIC_ALICE_SECRET"));
 println!("PROVED: anonymous different-device caller cannot see private note in prompt, but workmem.get({{}}) returns it.");
 futures::executor::block_on(reg.call("workmem.set",json!({"scope_person":"alice","scope_device":"dot2-private","key_or_topic":"private_note","content":"AUDIT_OVERWRITTEN"}),&ctx));
 assert_eq!(wm.get("alice","dot2-private","private_note").unwrap().unwrap().content,"AUDIT_OVERWRITTEN");
 println!("PROVED: same anonymous caller overwrites another person's device-scoped note.");
 futures::executor::block_on(reg.call("workmem.delete",json!({"scope_person":"alice","scope_device":"dot2-private","key_or_topic":"private_note"}),&ctx));
 assert!(wm.get("alice","dot2-private","private_note").unwrap().is_none());
 println!("PROVED: same anonymous caller deletes another person's device-scoped note.");
 let socket_store=Arc::new(Store::open_in_memory().unwrap());
 let rt=hm_core::tool::open_socket::OpenSocketRuntime::new(socket_store);
 let contract:hm_core::tool::open_socket::OpenSocketContract=serde_json::from_value(json!({"name":"family_light.start","description":"Audit synthetic tool","effect":"Query","endpoint_url":"http://audit.invalid/example","method":"GET"})).unwrap();
 rt.register(contract,None,"audit").unwrap();
 reg.add_provider(rt);
 assert!(reg.names().iter().any(|n|n=="family_light.start"));
 let decoded=hm_core::llm::from_wire_tool_name(&hm_core::llm::to_wire_tool_name("family_light.start"));
 let unknown=futures::executor::block_on(reg.call(&decoded,json!({}),&ctx));
 assert_eq!(unknown.error.unwrap().code,"UNKNOWN_TOOL");
 println!("PROVED: valid registered dynamic tool family_light.start cannot be resolved after model wire name round-trip (zero HTTP sent).");
}
```

## 记忆与动态名称输出

```text
PROVED: anonymous different-device caller cannot see private note in prompt, but workmem.get({}) returns it.
PROVED: same anonymous caller overwrites another person's device-scoped note.
PROVED: same anonymous caller deletes another person's device-scoped note.
PROVED: valid registered dynamic tool family_light.start cannot be resolved after model wire name round-trip (zero HTTP sent).
```

## 音频复现源码

```rust
#[cfg(test)]
mod tests {
    use home_mate_device_gateway::firmware::{aec::dealias::{Dealias, LockState}, nearend::{NearEndEvaluator, kws_guard_should_suppress}};
    use home_mate_device_gateway::protocol::EchoPathHealth;
    use std::sync::{Arc, Mutex};

    #[test]
    fn zeroed_reference_during_nonzero_playback_is_reported_healthy() {
        let d = Dealias::new();
        let mut seed = 12345u64;
        let mut mono = Vec::new();
        for _ in 0..4800 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            mono.push(((seed >> 33) as i32 % 8000) as i16);
        }
        let stereo:Vec<i16> = mono.iter().flat_map(|v| [*v,*v]).collect();
        d.feed(&stereo);
        let first:Vec<i16> = (0..320).map(|i| mono[300+3*i]).collect();
        assert_eq!(d.clean(&first,&[]).state, LockState::Locked);
        assert!(mono[1260..2220].iter().any(|v| *v != 0));
        let missing = d.clean(&vec![0;320],&[]);
        assert_eq!(missing.state,LockState::Locked, "Current code incorrectly preserves a verified-block label");
        let mut eval = NearEndEvaluator::new("test",true,true);
        let (health,_) = eval.update_echo_health(missing.state==LockState::Locked,true,1000);
        assert_eq!(health,EchoPathHealth::Healthy);
        assert_eq!(eval.guard_corr(true,0.0),0.0);
    }

    #[test]
    fn missing_barge_detector_bypasses_degraded_kws_guard() {
        let nearend=Arc::new(Mutex::new(NearEndEvaluator::new("test",true,true)));
        assert_eq!(nearend.lock().unwrap().update_echo_health(false,true,1000).0,EchoPathHealth::Degraded);
        assert!(!kws_guard_should_suppress(true,&nearend,None,0.5,1000), "Current code fails open without optional barge detector");
    }

    #[test]
    fn speech_present_at_startup_becomes_noise_floor() {
        let mut eval=NearEndEvaluator::new("test",true,true);
        let speech:Vec<i16>=(0..1280).map(|i| (250.0*(2.0*std::f32::consts::PI*440.0*i as f32/16000.0).sin()) as i16).collect();
        for i in 1..20 {
            let out=eval.evaluate(&speech,false,true,None,false,i*80,i*80);
            assert!(!out.assessment.present);
            assert!(out.assessment.snr_db.abs()<0.01);
            assert!(out.assessment.rms_dbfs > -50.0);
        }
    }
}
```

## 音频复现 Cargo 配置

```toml
[package]
name = "hm-audit-audio-repro"
version = "0.0.0"
edition = "2021"
[dependencies]
home-mate-device-gateway = { path = "/path/to/home-mate-device-gateway-b/crates/gateway" }
```

## UI 两个合成探针输出

实际提取 Vue 计算式和 watcher 回调，以 Vue refs 执行，无 API/浏览器访问。

```text
WorkingMemory: {enabled:false,upload:false,url:'http://old.example'}
Fleet initial: {serverId:'audit-node',renderedInput:'',dirty:true}
```

前者显示新草稿被旧服务端快照覆盖；后者显示非空服务器身份未初始化进输入。完整复现步骤、实际表达式来源与修复后验收见 [UI 报告](ui.md)。
