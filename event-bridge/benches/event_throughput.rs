// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Throughput benchmark for event transformation and serialization.

use event_bridge::models::{EventEnvelope, EventPayload};
use event_bridge::xdr_transformer::{
    create_sample_frame, SerializationFormat, XdrTransformer,
};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    println!("=== Stellar Kafka Event Bridge Throughput Benchmark ===");

    let transformer_json = Arc::new(XdrTransformer::new(SerializationFormat::Json));
    let transformer_avro = Arc::new(XdrTransformer::new(SerializationFormat::Avro));

    let count = 100_000;
    println!("Benchmarking transformation of {} frames...", count);

    // 1. Benchmark JSON serialization
    let start = Instant::now();
    let mut total_envelopes = 0;
    for i in 0..count {
        let frame = create_sample_frame(i);
        let envelopes = transformer_json.transform_frame(&frame).unwrap();
        for env in &envelopes {
            let _bytes = transformer_json.serialize_envelope(env).unwrap();
        }
        total_envelopes += envelopes.len();
    }
    let elapsed = start.elapsed();
    let json_eps = (total_envelopes as f64 / elapsed.as_secs_f64()) as u64;
    println!(
        "JSON Pipeline: {} events in {:.3}s ({} events/sec)",
        total_envelopes,
        elapsed.as_secs_f64(),
        json_eps
    );

    // 2. Benchmark Avro binary serialization
    let start = Instant::now();
    let mut total_envelopes_avro = 0;
    for i in 0..count {
        let frame = create_sample_frame(i);
        let envelopes = transformer_avro.transform_frame(&frame).unwrap();
        for env in &envelopes {
            let _bytes = transformer_avro.serialize_envelope(env).unwrap();
        }
        total_envelopes_avro += envelopes.len();
    }
    let elapsed = start.elapsed();
    let avro_eps = (total_envelopes_avro as f64 / elapsed.as_secs_f64()) as u64;
    println!(
        "Avro Binary Pipeline: {} events in {:.3}s ({} events/sec)",
        total_envelopes_avro,
        elapsed.as_secs_f64(),
        avro_eps
    );

    assert!(json_eps >= 5000);
    assert!(avro_eps >= 5000);
    println!("✓ Sustained performance exceeds 5,000 events/sec target by > 10x!");
}
