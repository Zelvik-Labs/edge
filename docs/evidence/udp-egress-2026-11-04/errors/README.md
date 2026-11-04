# UDP 出口错误载荷组件

Actions 41773249491，harness dd56643ae69331d4ad85fc2ff300e69f3d5c7329。
下载 artifact 13295453856，保留 raw.zip、artifact.json、manifest.json、extracted/。

```sh
python3 docs/evidence/udp-egress-2026-11-04/verify.py errors DOWNLOAD
```

核验所有原始归档字节、实际二进制、24 个版本/场景控制、分配记录与 3840 计时块。
外层错误仍分配40字节；borrowed拒绝仍复制正文，不能称零分配或消除失败复制。
冻结当时构建器，后续构建器另检查与实际源码区域逐字一致。未据此声称节点或真实AF_XDP吞吐提升。
