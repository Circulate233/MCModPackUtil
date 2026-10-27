use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use mcmpu::curseforge::client::CurseForgeClient;
use mcmpu::curseforge::resolver::CurseForgeApi;
use mcmpu::net::{NetworkConfig, NetworkEngine};

#[test]
fn client_sends_api_key_and_maps_project_json() {
    let body = r#"{
      "data": {
        "id": 873867,
        "name": "Red Core",
        "slug": "red-core",
        "classId": 6,
        "clientSide": 1,
        "serverSide": "optional",
        "authors": [{"name": "Desoroxxx"}]
      }
    }"#;
    let server = TestServer::responding(200, body);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let project = client.get_project(873867).unwrap();
    let request = server.join();

    assert_eq!(project.id, 873867);
    assert_eq!(project.name, "Red Core");
    assert_eq!(project.slug, "red-core");
    assert_eq!(project.class_id, 6);
    assert!(project.has_client_side_support());
    assert!(project.has_server_side_support());
    assert_eq!(project.author_names(), vec!["Desoroxxx"]);
    assert!(request.contains("GET /v1/mods/873867 HTTP/1.1"));
    assert!(has_api_key(&request));
}

#[test]
fn client_searches_projects_with_filter_and_page_limit() {
    let body = r#"{
      "data": [
        {
          "id": 521480,
          "name": "Skin Layers 3D",
          "slug": "skin-layers-3d",
          "classId": 6,
          "authors": [{"name": "tr7zw"}]
        }
      ]
    }"#;
    let server = TestServer::responding(200, body);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let projects = client.search_projects("skin layers").unwrap();
    let request = server.join();

    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, 521480);
    assert_eq!(projects[0].slug, "skin-layers-3d");
    assert_eq!(projects[0].author_names(), vec!["tr7zw"]);
    assert!(request.contains("GET /v1/mods/search?"));
    assert!(request.contains("gameId=432"));
    assert!(request.contains("searchFilter=skin+layers"));
    assert!(request.contains("pageSize=20"));
    assert!(has_api_key(&request));
}

#[test]
fn client_maps_file_json_to_internal_model() {
    let body = r#"{
      "data": {
        "id": 6147102,
        "modId": 873867,
        "fileName": "!Red-Core-MC-1.8-1.12-0.6.jar",
        "displayName": "Red Core 0.6",
        "isAvailable": true,
        "fileStatus": 4,
        "releaseType": 2,
        "gameVersions": ["Client", "Server", "Forge", "1.12.2"],
        "dependencies": [{"modId": 419286, "relationType": 3}],
        "downloadUrl": "https://edge.forgecdn.net/files/6147/102/file.jar",
        "hashes": [{"value": "0123456789abcdef0123456789abcdef01234567", "algo": 1}]
      }
    }"#;
    let server = TestServer::responding(200, body);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let file = client.get_file(873867, 6147102).unwrap();
    let request = server.join();

    assert_eq!(file.id, 6147102);
    assert_eq!(file.mod_id, 873867);
    assert_eq!(file.file_name, "!Red-Core-MC-1.8-1.12-0.6.jar");
    assert!(file.has_client_tag());
    assert!(file.has_server_tag());
    assert_eq!(file.dependencies[0].mod_id, 419286);
    assert_eq!(
        file.download_url.as_deref(),
        Some("https://edge.forgecdn.net/files/6147/102/file.jar")
    );
    assert!(request.contains("GET /v1/mods/873867/files/6147102 HTTP/1.1"));
}

#[test]
fn client_omits_mod_loader_type_when_loader_filter_is_empty() {
    let body = r#"{
      "data": []
    }"#;
    let server = TestServer::responding(200, body);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let files = client.list_files(521480, "1.20.1", "").unwrap();
    let request = server.join();

    assert!(files.is_empty());
    assert!(request.contains("GET /v1/mods/521480/files?"));
    assert!(request.contains("gameVersion=1.20.1"));
    assert!(!request.contains("modLoaderType="));
}

#[test]
fn client_sends_mod_loader_type_when_loader_filter_is_explicit() {
    let body = r#"{
      "data": []
    }"#;
    let server = TestServer::responding(200, body);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let files = client.list_files(521480, "1.20.1", "neoforge").unwrap();
    let request = server.join();

    assert!(files.is_empty());
    assert!(request.contains("gameVersion=1.20.1"));
    assert!(request.contains("modLoaderType=6"));
}

#[test]
fn client_reports_non_success_status_as_api_error() {
    let server = TestServer::responding(403, r#"{"error":"forbidden"}"#);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let err = client.get_project(873867).unwrap_err();
    let _request = server.join();

    assert!(err.to_string().contains("403"));
}

#[test]
fn client_accepts_explicit_proxy_configuration() {
    let client = CurseForgeClient::try_with_base_url_and_proxy(
        "https://api.curseforge.com".to_string(),
        "secret-key".to_string(),
        Some("http://127.0.0.1:7890"),
    );

    assert!(client.is_ok());
}

#[test]
fn client_rejects_invalid_proxy_configuration() {
    let result = CurseForgeClient::try_with_base_url_and_proxy(
        "https://api.curseforge.com".to_string(),
        "secret-key".to_string(),
        Some("not a proxy url"),
    );

    let Err(err) = result else {
        panic!("invalid proxy should fail");
    };
    assert!(err.to_string().contains("invalid proxy URL"));
    assert!(!err.to_string().contains("secret-key"));
}

#[test]
fn client_rejects_json_larger_than_the_api_response_limit() {
    let server = TestServer::responding_with_declared_length(17 * 1024 * 1024);
    let client = CurseForgeClient::with_base_url(server.base_url(), "secret-key".to_string());

    let err = client.get_project(873867).unwrap_err();
    let _request = server.join();

    assert!(err.to_string().contains("exceeded"));
    assert!(err.to_string().contains("byte limit"));
}

#[test]
fn client_does_not_forward_api_key_to_a_redirected_host() {
    let destination = TcpListener::bind("127.0.0.1:0").unwrap();
    let destination_address = destination.local_addr().unwrap();
    let destination_handle = thread::spawn(move || {
        let (mut stream, _) = destination.accept().unwrap();
        let request = read_request(&mut stream);
        let body = r#"{"data":{"id":873867,"name":"Red Core","slug":"red-core","classId":6,"authors":[]}}"#;
        write_response(&mut stream, 200, "OK", body, &[]);
        request
    });

    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let source_address = source.local_addr().unwrap();
    let source_handle = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        let request = read_request(&mut stream);
        write_response(
            &mut stream,
            302,
            "Found",
            "",
            &[(
                "Location",
                format!("http://localhost:{}/redirected", destination_address.port()),
            )],
        );
        request
    });

    let client = CurseForgeClient::with_base_url(
        format!("http://{source_address}"),
        "secret-key".to_string(),
    );
    let project = client.get_project(873867).unwrap();
    let source_request = source_handle.join().unwrap();
    let destination_request = destination_handle.join().unwrap();

    assert_eq!(project.id, 873867);
    assert!(has_api_key(&source_request));
    assert!(!has_api_key(&destination_request));
}

#[test]
fn cloned_clients_reuse_an_injected_network_engine() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let body = r#"{"data":{"id":873867,"name":"Red Core","slug":"red-core","classId":6,"authors":[]}}"#;
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            requests.push(read_request(&mut stream));
            write_response(&mut stream, 200, "OK", body, &[]);
        }
        requests
    });
    let network = NetworkEngine::new(NetworkConfig::default()).unwrap();
    let client = CurseForgeClient::try_with_base_url_and_network(
        format!("http://{address}"),
        "secret-key".to_string(),
        network,
    )
    .unwrap();

    client.get_project(873867).unwrap();
    client.clone().get_project(873867).unwrap();
    let requests = server.join().unwrap();

    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| has_api_key(request)));
}

struct TestServer {
    base_url: String,
    handle: thread::JoinHandle<String>,
}

impl TestServer {
    fn responding(status: u16, body: &'static str) -> Self {
        Self::responding_owned(status, body.to_string())
    }

    fn responding_owned(status: u16, body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let status_text = match status {
                200 => "OK",
                403 => "Forbidden",
                _ => "Error",
            };
            write_response(&mut stream, status, status_text, &body, &[]);
            request
        });

        Self {
            base_url: format!("http://{address}"),
            handle,
        }
    }

    fn responding_with_declared_length(content_length: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        Self {
            base_url: format!("http://{address}"),
            handle,
        }
    }

    fn base_url(&self) -> String {
        self.base_url.clone()
    }

    fn join(self) -> String {
        self.handle.join().unwrap()
    }
}

/// Returns whether a raw request head carries the CurseForge API key header.
///
/// Header names are case-insensitive, so the wire casing must not decide the result.
fn has_api_key(request: &str) -> bool {
    request
        .lines()
        .any(|line| line.eq_ignore_ascii_case("x-api-key: secret-key"))
}

fn read_request(stream: &mut impl Read) -> String {
    let mut buffer = [0_u8; 8192];
    let read = stream.read(&mut buffer).unwrap();
    String::from_utf8_lossy(&buffer[..read]).to_string()
}

fn write_response(
    stream: &mut impl Write,
    status: u16,
    status_text: &str,
    body: &str,
    headers: &[(&str, String)],
) {
    let extra_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).unwrap();
}
