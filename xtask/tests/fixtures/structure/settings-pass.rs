// let listen_port = 8080;
const EXAMPLE: &str = "let retry_count = 3;";
const RAW_EXAMPLE: &str = r#"let storage_path = "data/pool";"#;
const ARRAY_LENGTH: usize = 16;
let important = 42;
let passport = 7;
let retry_count = defaults.retry_count;
let listen_port = settings::PORT;
let timeout_secs = Duration::from_secs(2);
let storage_path = PathBuf::from("data/pool");
let (port, path) = (8080, "data/pool");
let member = MemberId(7);
let retry_count = 1 + 2;
let retry_count = (3);
let path = 7;
let port = "service";
struct Options { port: u16 }
let options = Options { port: 8080 };
fn port() -> u16 { 8080 }
macro_rules! ignored { () => { let retry_count = 3; }; }
ignored!(let listen_port = 8080;);
let path = b"data/pool";
let path = c"data/pool";
let port = true;
let port = 'x';
/* outer /* nested */ let retry_count = 3; */
