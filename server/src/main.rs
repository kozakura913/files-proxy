use std::{collections::HashMap, io::Write, net::SocketAddr, sync::{atomic::{AtomicBool, AtomicU32}, Arc}, time::Duration};

use axum::{body::StreamBody, response::IntoResponse, routing::get, Router};
use serde::{Deserialize, Serialize};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{tcp::{OwnedReadHalf, OwnedWriteHalf},TcpStream}, sync::{mpsc::{error::TryRecvError, Receiver, Sender}, Mutex}};

#[derive(Serialize,Deserialize,Debug)]
struct ConfigFile{
	http_bind_addr:String,//0.0.0.0:8080
	agent_bind_addr:String,//0.0.0.0:23725
	ping_wait_ms:u64,//0でping無し 標準30000
	ping_worker_timeout:u32,//3000
	ping_worker_poll:u16,//5
	http_agentwait_timeout:u32,//3000
	http_agentwait_poll:u16,//5
	agent_queue_send_poll:u16,//5
}

fn main() {
	if !std::path::Path::new("config.json").exists(){
		let default_config=ConfigFile{
			http_bind_addr: "0.0.0.0:8080".to_owned(),
			agent_bind_addr: "0.0.0.0:23725".to_owned(),
			ping_wait_ms: 30000,
			ping_worker_timeout: 3000,
			ping_worker_poll: 5,
			http_agentwait_timeout: 3000,
			http_agentwait_poll: 5,
			agent_queue_send_poll: 5,
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create("config.json").unwrap().write_all(default_config.as_bytes()).unwrap();
	}
	let config:ConfigFile=serde_json::from_reader(std::fs::File::open("config.json").unwrap()).unwrap();
	let config=Arc::new(config);
	let (send_queue,recv_queue)=tokio::sync::mpsc::channel::<RequestJob>(4);
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	async fn tcp_listener(config:Arc<ConfigFile>,mut recv_queue:Receiver<RequestJob>){
		let tcp_addr:SocketAddr = config.agent_bind_addr.parse().unwrap();
		match tokio::net::TcpListener::bind(tcp_addr).await{
			Ok(con) => {
				loop{
					let stream=con.accept().await;
					recv_queue=agent_worker(stream.unwrap(),recv_queue,config.clone()).await;
				}
			},
			Err(e) => {
				eprintln!("{:?}",e);
			},
		}
	}
	rt.spawn(tcp_listener(config.clone(),recv_queue));
	let send_queue=Arc::new(send_queue);
	if config.ping_wait_ms>0{
		let send_queue0=send_queue.clone();
		rt.spawn(ping_worker(send_queue0,config.clone()));
	}
	rt.block_on(http_server(send_queue,config));
}
async fn ping_worker(send_queue: Arc<Sender<RequestJob>>,config:Arc<ConfigFile>){
	loop{
		tokio::time::sleep(Duration::from_millis(config.ping_wait_ms)).await;
		let start_time=tokio::time::Instant::now();
		let (send_result,mut recv_result)=tokio::sync::mpsc::channel(1);
		if let Err(e)=send_queue.send(RequestJob{
			type_id:3,
			localpath: None,
			result:Some(send_result),
		}).await{
			eprintln!("Agent Ping {}",e);
		}
		let mut res=None;
		for _ in 0..config.ping_worker_timeout as i64/config.ping_worker_poll as i64{
			match recv_result.try_recv(){
				Ok(res0)=>{
					res.replace(res0);
					break;
				},
				Err(TryRecvError::Empty)=>{
					tokio::time::sleep(Duration::from_millis(config.ping_worker_poll.into())).await;
				},
				Err(TryRecvError::Disconnected)=>{
					eprintln!("Agent Ping Disconnected");
					break;
				}
			}
		}
		let ping_time=tokio::time::Instant::now()-start_time;
		let is_ok=res.as_ref().map(|_|"ok").unwrap_or("error");
		println!("LastAgentPing {}ms {}",ping_time.as_millis(),is_ok);
	}
}
async fn http_server(send_queue: Arc<Sender<RequestJob>>,config:Arc<ConfigFile>){
	let http_addr:SocketAddr = config.http_bind_addr.parse().unwrap();
	let client=reqwest::Client::new();
	let app = Router::new().route("/",get(||async move{
		axum::http::StatusCode::NO_CONTENT
	})).route("/*path",get(|axum::extract::Path(path):axum::extract::Path<String>|async move{
		let type_id=if path.starts_with("webpublic-"){
			1
		}else if path.starts_with("thumbnail-"){
			2
		}else{
			1//wip
		};
		//println!("{}",path);
		let (send_result,mut recv_result)=tokio::sync::mpsc::channel(1);
		let job=RequestJob{
			type_id,
			localpath:Some(path),
			result:Some(send_result),
		};
		match send_queue.send_timeout(job,Duration::from_millis(3000)).await{
			Ok(_) => {

			},
			Err(e) => {
				let event_id=uuid::Uuid::new_v4().to_string();
				eprintln!("EventId[{}] job send error {:?}",event_id,e);
				let value=event_id.parse().unwrap();
				let mut resp=(axum::http::StatusCode::TOO_MANY_REQUESTS).into_response();
				resp.headers_mut().append("X-EventId",value);
				return Err(resp);
			},
		}
		let mut res=None;
		for _ in 0..config.http_agentwait_timeout as i64/config.http_agentwait_poll as i64{
			match recv_result.try_recv(){
				Ok(res0)=>{
					res.replace(res0);
					break;
				},
				Err(TryRecvError::Empty)=>{
					tokio::time::sleep(Duration::from_millis(config.http_agentwait_poll.into())).await;
				},
				Err(TryRecvError::Disconnected)=>{
					return Err((axum::http::StatusCode::GATEWAY_TIMEOUT,"").into_response());
				}
			}
		}
		recv_result.close();
		let res=match res{
			Some(res)=>res,
			None=>return Err((axum::http::StatusCode::GATEWAY_TIMEOUT,"").into_response())
		};
		if res.status!=200{
			let event_id=uuid::Uuid::new_v4().to_string();
			eprintln!("EventId[{}] job Agent Status {}",event_id,res.status);
			let value=event_id.parse().unwrap();
			let mut resp=(axum::http::StatusCode::BAD_GATEWAY).into_response();
			resp.headers_mut().append("X-EventId",value);
			resp.headers_mut().append("X-AgentStatus",res.status.to_string().parse().unwrap());
			return Err(resp);
		}
		if res.json.is_none(){
			return Err((axum::http::StatusCode::NO_CONTENT).into_response());
		};
		let json=serde_json::from_str::<ResponseJson>(&res.json.as_ref().unwrap());
		let json=match json{
			Ok(data)=>data,
			Err(e)=>{
				let event_id=uuid::Uuid::new_v4().to_string();
				eprintln!("EventId[{}] job Agent Json Parse error {:?}",event_id,e);
				let value=event_id.parse().unwrap();
				let mut resp=(axum::http::StatusCode::BAD_GATEWAY).into_response();
				resp.headers_mut().append("X-EventId",value);
				return Err(resp);
			}
		};
		println!("GET {}",json.uri);
		let request=client.get(json.uri).build();
		let request=match request{
			Ok(request)=>request,
			Err(e)=>return Err((axum::http::StatusCode::BAD_GATEWAY,format!("{:?}",e)).into_response())
		};
		let resp=client.execute(request).await;
		let resp=match resp{
			Ok(resp)=>resp,
			Err(e)=>return Err((axum::http::StatusCode::BAD_GATEWAY,format!("{:?}",e)).into_response())
		};
		let status=resp.status();
		let body=StreamBody::new(resp.bytes_stream());
		let mut headers=axum::headers::HeaderMap::new();
		headers.append("X-RemoteStatus",status.as_u16().to_string().parse().unwrap());
		if status.is_success(){
			Ok((axum::http::StatusCode::OK,headers,body))
		}else{
			Ok((axum::http::StatusCode::BAD_GATEWAY,headers,body))
		}
	}));
	axum::Server::bind(&http_addr).serve(app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
}
async fn agent_worker((tcp,_addr):(TcpStream,SocketAddr),mut recv_queue:Receiver<RequestJob>,config:Arc<ConfigFile>)->Receiver<RequestJob>{
	println!("start agent session");
	let id=AtomicU32::new(0);
	async fn req(con:&mut OwnedWriteHalf,req:&RequestJob,id:u32)->Result<(),std::io::Error>{
		con.write_u8(req.type_id).await?;//種別
		con.write_u8(0u8).await?;//padding
		con.write_u32(id).await?;//id
		if let Some(url_string)=req.localpath.as_ref(){
			let url_string=url_string.as_bytes();
			con.write_u16(url_string.len() as u16+2).await?;
			con.write_u16(url_string.len() as u16).await?;
			con.write_all(url_string).await?;
		}else{
			con.write_u16(0).await?;
		}
		con.flush().await?;
		Ok(())
	}
	async fn res(con:&mut OwnedReadHalf)->Result<AgentResult,std::io::Error>{
		let status=con.read_u16().await?;
		let id=con.read_u32().await?;
		//println!("eventid {}",id);
		let len=con.read_u16().await? as usize;
		let s=if len>0{
			let mut buf=Vec::with_capacity(len);
			unsafe{
				buf.set_len(len);
			}
			con.read_exact(&mut buf).await?;
			String::from_utf8(buf).map(|s|Some(s)).unwrap_or_else(|_|None)
		}else{
			None
		};
		Ok(AgentResult{
			id,
			status,
			json:s,
		})
	}
	let working_buffer=Arc::new(Mutex::new(HashMap::new()));
	let (mut reader,mut writer)=tcp.into_split();
	let working_buffer0=working_buffer.clone();
	let is_error=Arc::new(AtomicBool::new(false));
	let is_error0=is_error.clone();
	tokio::runtime::Handle::current().spawn(async move{
		loop{
			match res(&mut reader).await{
				Ok(res)=>{
					let id=res.id;
					let sender:Option<Sender<AgentResult>>=working_buffer.lock().await.remove(&id);
					match sender{
						Some(sender)=>{
							//println!("RESPONSE={:?}",res);
							if let Err(e)=sender.send(res).await{
								eprintln!("AgentResult SendError {}",e);
							}
						},
						None=>{
							eprintln!("Drop Agent Response {}",id);
						}
					}
				},
				Err(e)=>{
					eprintln!("agent session error {:?}",e);
					is_error0.store(true,std::sync::atomic::Ordering::Relaxed);
					break;
				}
			}
		}
	});
	loop{
		let mut res=None;
		loop{
			if is_error.load(std::sync::atomic::Ordering::Relaxed){
				break;
			}
			match recv_queue.try_recv(){
				Ok(res0)=>{
					res.replace(res0);
					break;
				},
				Err(TryRecvError::Empty)=>{
					//job wait
					tokio::time::sleep(Duration::from_millis(config.agent_queue_send_poll.into())).await;
				},
				Err(TryRecvError::Disconnected)=>{
					break;
				}
			}
		}
		if let Some(mut job)=res{
			let id=id.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
			if let Some(res)=job.result.take(){
				working_buffer0.lock().await.insert(id,res);
			}
			if let Err(e)=req(&mut writer,&job,id).await{
				eprintln!("agent session error {:?}",e);
				match writer.shutdown().await{
					Ok(_)=>{
						println!("shutdown agent session");
					},
					Err(e)=>{
						eprintln!("shutdown {:?}",e);
					}
				}
			}
		}else{
			break;
		}
	}
	println!("exit agent session");
	return recv_queue;
}
struct RequestJob{
	type_id:u8,
	localpath:Option<String>,
	result:Option<Sender<AgentResult>>,
}
#[derive(Debug)]
struct AgentResult{
	id:u32,
	status:u16,
	json:Option<String>,
}
#[derive(Deserialize,Debug)]
struct ResponseJson{
	uri:String,
}
