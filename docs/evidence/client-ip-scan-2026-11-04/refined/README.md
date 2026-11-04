# 调整后的客户端 IP 组件

Actions 41772781532，harness 95f820fcdc716340f1ff795e9c7f3d35b66f43d9。
下载 artifact 13295990766，保留 raw.zip、artifact.json、manifest.json、extracted/。

```sh
python3 docs/evidence/client-ip-scan-2026-11-04/verify.py refined DOWNLOAD
```

核验所有原始归档字节、实际二进制、75,923 组差分、分配记录与 6080 计时块。
冻结原始构建器以确保旧结果不依赖后续集成断言；候选入口之外的实际函数已在 20e56ff 对应，后续组件归档再保留其源码。
组件不包含真实 Pingora IO；19 种条件的退步与全部配对比值保留。
