Recorded with `iptables-save -t filter` (iptables 1.8.10, nf_tables) in a throwaway Alpine 3.20
container with NET_ADMIN. Rules were made with iptables and ipset commands; no real address,
domain or code of another pool is involved. All files are recorded; the circular chains and the
cut-off cases in tests/port_filter.rs are inline text.
