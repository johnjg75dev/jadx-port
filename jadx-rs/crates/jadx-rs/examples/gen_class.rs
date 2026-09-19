//! Writes a small but complete `T.class` file, so the pipeline can be exercised
//! without a JDK (`javac` is not always available, e.g. in CI for this port).
//!
//!	```text
//!	cargo run -p jadx-rs --example gen_class -- /tmp/T.class
//!	```
//!
//! The generated class is:
//!
//! ```java
//! package com.example;
//!
//! public class Generator {
//!     public static final int LIMIT = 10;
//!
//!     public static int abs(int v) {
//!         if (v < 0) {
//!             v = -v;
//!         }
//!         return v;
//!     }
//! }
//! ```
//!
//! (named `com/example/Generator`, written to the path given on the command line).

use jadx_rs::testutil::{ClassBuilder, CodeBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let out = std::env::args().nth(1).unwrap_or_else(|| "T.class".to_string());
	let mut b = ClassBuilder::new("com/example/Generator", "java/lang/Object");
	b.source_file("Generator.java");
	let limit = b.int_const(10);
	b.field_const(0x0019, "LIMIT", "I", limit); // public static final

	let mut c = CodeBuilder::new(2, 2);
	let end = c.new_label();
	c.op(0x1a); // iload_0
	c.branch(0x9c, end); // ifge end
	c.op(0x1a); // iload_0
	c.op(0x74); // ineg
	c.op(0x3b); // istore_0
	c.mark(end);
	c.op(0x1a); // iload_0
	c.op(0xac); // ireturn
	b.method(0x0009, "abs", "(I)I", Some(c));

	let bytes = b.to_bytes();
	std::fs::write(&out, &bytes)?;
	eprintln!("wrote {} bytes to {}", bytes.len(), out);
	Ok(())
}
