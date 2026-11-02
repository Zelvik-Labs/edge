"""Add regression coverage only after the unmodified baseline probe exits."""
import hashlib,json,os,re,shutil,subprocess,time
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-retry-buffer';deadline=time.monotonic()+45*60
while time.monotonic()<deadline:
    if 'Baseline warning reproduced' in (local/'probe-before.log').read_text():break
    time.sleep(5)
else:raise TimeoutError('Baseline probe did not complete')
m=json.loads((local/'before/metadata.json').read_text());probe=json.loads((local/'before/probe-completed.json').read_text())
assert probe['returncode']==0 and probe['native_errors']==0 and probe['warning_count']>0
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
for n,h in m['source_sha256'].items():assert sha(root/n)==h,n
h3=r'''    #[tokio::test]
    async fn h3_retry_regression_trailers_keep_the_body_end_marker() {
        let (mut session, mut client, _pair) = pair(get(None)).await;
        session.enable_retry_buffering();
        let mut trailers = HeaderMap::new();
        trailers.insert("authorization", "trailer-only".parse().unwrap());
        client.send_trailers(trailers).await.unwrap();
        client.finish().await.unwrap();
        wait_for_probe(&mut session).await;
        assert!(session.is_body_done());
        assert!(!session.is_body_empty());
        // Pingora needs an initial pipe task when FIN was already consumed
        // and trailers prohibit the body-empty cache-admission shortcut.
        assert!(session.get_retry_buffer().unwrap().is_empty());
    }

    #[tokio::test]
    async fn h3_retry_regression_truncation_never_exposes_a_partial_replay() {
        let (mut session, mut client, _pair) = pair(get(None)).await;
        session.enable_retry_buffering();
        let prefix = Bytes::from(vec![0x5a; 64 * 1024]);
        client.send_data(prefix.clone()).await.unwrap();
        let mut received = Vec::new();
        while received.len() < prefix.len() {
            let chunk = session.read_body_bytes().await.unwrap().unwrap();
            received.extend_from_slice(&chunk);
        }
        assert_eq!(session.get_retry_buffer().unwrap(), prefix);
        assert!(!session.retry_buffer_truncated());
        client.send_data(Bytes::from_static(b"x")).await.unwrap();
        assert_eq!(session.read_body_bytes().await.unwrap().unwrap().as_ref(), b"x");
        assert!(session.retry_buffer_truncated());
        assert!(session.get_retry_buffer().is_none());
        assert!(session.retry_buffer.as_ref().unwrap().is_empty());
        client.send_data(Bytes::from_static(b"tail")).await.unwrap();
        client.finish().await.unwrap();
        while session.read_body_bytes().await.unwrap().is_some() {
            assert!(session.get_retry_buffer().is_none());
            assert!(session.retry_buffer.as_ref().unwrap().is_empty());
        }
        assert_eq!(session.body_bytes_read(), prefix.len() + 5);
        assert!(session.retry_buffer_truncated());
        assert!(session.get_retry_buffer().is_none());
        session.enable_retry_buffering();
        assert!(!session.retry_buffer_truncated());
        assert!(session.get_retry_buffer().unwrap().is_empty());
    }

'''
proxy=r'''    #[cfg(unix)]
    #[tokio::test]
    async fn h3_retry_regression_final_empty_chunk_is_a_body_end() {
        use bytes::Bytes;
        use pingora_proxy::ProxyHttp;
        let (_client, downstream) = tokio::net::UnixStream::pair().unwrap();
        let mut session = pingora::proxy::Session::new_h1(Box::new(
            pingora::protocols::l4::stream::Stream::from(downstream),
        ));
        let proxy = test_proxy();
        let mut ctx = super::ProxyCTX {
            is_http3_downstream: true,
            ..Default::default()
        };
        for (input, end, expected) in [
            (Some(Bytes::new()), false, Some(Bytes::new())),
            (Some(Bytes::from_static(b"payload")), true, Some(Bytes::from_static(b"payload"))),
            (None, true, None),
            (Some(Bytes::new()), true, None),
        ] {
            let mut body = input;
            proxy.request_body_filter(&mut session, &mut body, end, &mut ctx).await.unwrap();
            assert_eq!(body, expected);
        }
    }

'''
for name,anchor,addition in [('src/h3_downstream.rs','    #[test]\n    fn h3_request_adapter_preserves_duplicate_headers()',h3),('src/proxy.rs','    fn uam_ua_hash(ua: &str) -> String {',proxy)]:
    p=root/name;text=p.read_text();assert text.count(anchor)==1 and 'h3_retry_regression_' not in text;p.write_text(text.replace(anchor,addition+anchor))
sources={n:sha(root/n) for n in m['source_sha256']}
snapshot=local/'red-source'
for n in ['src/h3_downstream.rs','src/proxy.rs']:
    p=snapshot/n;p.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(root/n,p)
cmd=['/workspace/docs/scripts/workspace/local-rust.sh','test','--locked','--lib','h3_retry_regression_']
with (local/'red-tests.log').open('x') as log:code=subprocess.run(cmd,env={**os.environ,'CARGO_TARGET_DIR':'/workspace/edge/target'},stdout=log,stderr=subprocess.STDOUT).returncode
log=(local/'red-tests.log').read_text();failed=sorted(re.findall(r'^test (\S+) \.\.\. FAILED',log,re.M));passed=sorted(re.findall(r'^test (\S+) \.\.\. ok',log,re.M))
result={'command':cmd,'returncode':code,'failed':failed,'passed':passed,'source_sha256':sources}
(local/'red-result.json').write_text(json.dumps(result,indent=2)+'\n')
assert code==101 and len(failed)==2 and len(passed)==1,(code,failed,passed)
assert any('truncation_never' in n for n in failed) and any('final_empty_chunk' in n for n in failed)
print('Expected old behavior failures reproduced',failed,flush=True)
