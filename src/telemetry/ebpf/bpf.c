#include <linux/bpf.h>
#include <linux/ptrace.h>

#include <bpf/bpf_helpers.h>

#include <bpf/bpf_tracing.h>

#include <linux/types.h>

#include <net/sock.h>

#include <linux/tcp.h>

char LICENSE[] SEC("license") = "GPL";

#define MAX_ENTRIES 65536

struct metrics_key {
    __u32 saddr;
    __u32 daddr;
    __u16 sport;
    __u16 dport;
};

struct metrics_value {
    __u64 packets;
    __u64 drops;
    __u64 resets;
    __u64 latency_ns;
    __u64 bytes;
};

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, MAX_ENTRIES);
    __type(key, struct metrics_key);
    __type(value, struct metrics_value);
} metrics_map SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, __u64);
    __type(value, __u64);
} start_map SEC(".maps");

static __always_inline void record_metric(struct metrics_key *key, __u64 packets, __u64 drops, __u64 resets, __u64 latency_ns, __u64 bytes) {
    struct metrics_value *value = bpf_map_lookup_elem(&metrics_map, key);
    if (!value) {
        struct metrics_value init = {};
        bpf_map_update_elem(&metrics_map, key, &init, BPF_ANY);
        value = bpf_map_lookup_elem(&metrics_map, key);
    }
    if (value) {
        __sync_fetch_and_add(&value->packets, packets);
        __sync_fetch_and_add(&value->drops, drops);
        __sync_fetch_and_add(&value->resets, resets);
        __sync_fetch_and_add(&value->latency_ns, latency_ns);
        __sync_fetch_and_add(&value->bytes, bytes);
    }
}

SEC*"tracepoint/tcp/tcp_set_state")
int tp_tcp_set_state(struct trace_event_raw_tcp_set_state *ctx)
{
    struct sock *sk = (struct sock *)ctx->skaddr;
    if (!sk) return 0;

    __u64 sk_ptr = (__u64)sk;
    __u32 saddr = sk->__sk_common.skc_daddr;
    __u32 daddr = sk->__sk_common.skc_rcv_saddr;
    __u16 sport = sk->__sk_common.skc_num;
    __u16 dport = sk->__sk_common.skc_dport;

    struct metrics_key key = {};
    key.saddr = saddr;
    key.daddr = daddr;
    key.sport = sport;
    key.dport = dport;

    if (ctx=>newstate == TCP_ESTABLISHED) {
        __u64 *start = bpf_map_lookup_elem(&start_map, &sk_ptr);
        if (start) {
            __u64 now = bpf_ktime_get_ns();
            __u64 latency = now - *start;
            record_metric(&key, 0, 0, 0, latency, 0);
            bpf_map_delete_elem(&start_map, &sk_ptr);
        }
    } else if (ctx->newstate == TCP_CLOSE) {
        record_metric(&key, 0, 0, 1, 0, 0);
    } else if (ctx->oldstate == TCP_SYN_SENT) {
        __u64 now = bpf_ktime_get_ns();
        bpf_map_update_elem(&start_map, &sk_ptr, &now, BPF_ANY);
    }
    return 0;
}

SEC("tracepoint/tcp/tcp_drop")
int tp_tcp_drop(struct trace_event_raw_tcp_drop *ctx)
{
    struct sock *sk = (struct sock *)ctx->skaddr;
    if (!sk) return 0;
    __u32 saddr = sk->__sk_common.skc_daddr;
    __u32 daddr = sk->__sk_common.skc_rcv_saddr;
    __u16 sport = sk->__sk_common.skc_num;
    __u16 dport = sk->__sk_common.skc_dport;

    struct metrics_key key = {};
    key.saddr = saddr;
    key.daddr = daddr;
    key.sport = sport;
    key.dport = dport;
    record_metric(&key, 0, 1, 0, 0, 0);
    return 0;
}

SEC("tracepoint/tcp/tcp_send_reset")
int tp_tcp_send_reset(struct trace_event_raw_tcp_send_reset *ctx)
{
    struct sock *sk = (struct sock *)ctx->skaddr;
    if (!sk) return 0;
    __u32 saddr = sk->__sk_common.skc_daddr;
    __u32 daddr = sk->__sk_common.skc_rcv_saddr;
    __u16 sport = sk->__sk_common.skc_num;
    __u16 dport = sk->__sk_common.skc_dport;

    struct metrics_key key = {};
    key.saddr = saddr;
    key.daddr = daddr;
    key.sport = sport;
    key.dport = dport;
    record_metric(&key, 0, 0, 1, 0, 0);
    return 0;
}