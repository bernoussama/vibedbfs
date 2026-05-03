# Agent Notes

- Do not use `/tmp` for filesystem benchmarks. On this machine `/tmp` is `tmpfs`
  and measures RAM-backed storage, not disk. Use a disk-backed directory under
  the repository, such as `.bench/`, unless the user explicitly asks for tmpfs.
