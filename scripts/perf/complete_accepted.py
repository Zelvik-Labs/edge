#!/usr/bin/env python3
"""Finish the original accepted responses after a recoverable pressure episode."""
import argparse
import asyncio
import hashlib
import importlib.util
import json
import pathlib

spec = importlib.util.spec_from_file_location("pressure_driver", str(pathlib.Path(__file__).with_name("memory_pressure.py")))
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


async def run(args):
    output = {"label": args.label, "source": args.acceptance_source}
    held, samples = {}, {}
    fill = p.H1()
    server = await asyncio.start_server(p.origin, "127.0.0.1", 19081)
    try:
        state = await p.control("stats")
        cg, pid = pathlib.Path(state["cgroup"]), state["pid"]
        output["before"] = {"state": state, "physical": p.observation(cg, pid)}
        for proto in ("h1", "h2", "h3"):
            held[proto] = p.H1() if proto == "h1" else await (p.H2().open() if proto == "h2" else p.H3().open())
            sample = await held[proto].request("/complete")
            samples[proto] = sample
            await asyncio.wait_for(sample.headers.wait(), 10)
            if sample.status != 200 or sample.error:
                raise RuntimeError(f"{proto} not accepted: {sample.status} {sample.error}")
        if args.acceptance_source in ("cache", "mixed"):
            output["preflight"] = await p.cache_preflight(fill, cg)
        before_bytes = {proto: s.bytes for proto, s in samples.items()}
        output["applied"] = await p.apply_acceptance_pressure(args, args.acceptance_source, 85, cg, fill)
        await asyncio.sleep(12)
        output["pressure"] = {
            "state": await p.control("stats"), "physical": p.observation(cg, pid),
            "held_new_requests": await p.probe_held(held, samples, 32),
            "accepted_progress": {proto: p.progress(s, before_bytes[proto]) for proto, s in samples.items()},
        }
        output["recovery"] = await p.recovery_checkpoint(held, samples, cg, pid)
        await asyncio.wait_for(asyncio.gather(*(s.done.wait() for s in samples.values())), 120)
        expected = hashlib.sha256(p.PAYLOAD * 2400).hexdigest()
        output["completion"] = {}
        for proto, sample in samples.items():
            output["completion"][proto] = {
                **p.progress(sample, 0), "sha256": sample.digest.hexdigest(),
                "expected_sha256": expected, "digest_matches": sample.digest.hexdigest() == expected,
            }
        output["after"] = {"state": await p.control("stats"), "physical": p.observation(cg, pid)}
        if not all(row["completed_normally"] and row["digest_matches"] for row in output["completion"].values()):
            raise RuntimeError("an accepted response failed completion or digest validation")
    except Exception as error:
        output["failure"] = repr(error)
        raise
    finally:
        pathlib.Path(args.output).write_text(json.dumps(output, indent=2))
        await p.control("release")
        for client in held.values():
            await client.close()
        await fill.close()
        server.close()
        await server.wait_closed()


parser = argparse.ArgumentParser()
parser.add_argument("--label", required=True)
parser.add_argument("--output", required=True)
parser.add_argument("--acceptance-source", required=True)
asyncio.run(run(parser.parse_args()))
