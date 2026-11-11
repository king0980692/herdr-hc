//! Two viewers on one scratch program: who sets its size, and whether a
//! terminal-generated reply counts as the viewer typing.
use super::*;

const A_SIZE: (u64, u64) = (100, 35);
const B_SIZE: (u64, u64) = (80, 24);
/// A primary device attributes reply, as a terminal emulator answers `CSI c`.
const AUTOMATIC_REPLY: &[u8] = b"\x1b[?62;22c";

async fn record(client: &Client, address: &str) -> io::Result<Value> {
    let list = client.call("terminal.list", json!({"all":true})).await?;
    list["terminals"]
        .as_array()
        .and_then(|terminals| terminals.iter().find(|t| t["address"] == address))
        .cloned()
        .ok_or_else(|| invalid("scratch program is missing from the source list"))
}

async fn size(client: &Client, address: &str) -> io::Result<(u64, u64)> {
    let record = record(client, address).await?;
    match (record["cols"].as_u64(), record["rows"].as_u64()) {
        (Some(cols), Some(rows)) => Ok((cols, rows)),
        _ => Err(invalid("source record has no size")),
    }
}

/// The source applies viewer frames asynchronously: wait for the expected size.
async fn expect_size(
    client: &Client,
    address: &str,
    expected: (u64, u64),
    step: &str,
) -> io::Result<()> {
    let mut last = (0, 0);
    for _ in 0..80 {
        last = size(client, address).await?;
        if last == expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(invalid(format!(
        "{step}: expected {expected:?}, source reports {last:?}"
    )))
}

/// Nothing may change the size during the settle window.
async fn expect_size_holds(
    client: &Client,
    address: &str,
    expected: (u64, u64),
    step: &str,
) -> io::Result<()> {
    for _ in 0..24 {
        let current = size(client, address).await?;
        if current != expected {
            return Err(invalid(format!(
                "{step}: size moved from {expected:?} to {current:?}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn drain_until(stream: &mut Stream, needle: &[u8], step: &str) -> io::Result<()> {
    timeout(Duration::from_secs(10), async {
        let mut output = Vec::new();
        loop {
            let frame = stream.recv().await?;
            if frame.kind == DATA {
                output.extend_from_slice(&frame.payload);
                stream.consumed(frame.payload.len()).await?;
                if output.windows(needle.len()).any(|part| part == needle) {
                    return Ok::<_, io::Error>(());
                }
            } else if frame.kind == CLOSE {
                return Err(disconnected());
            }
        }
    })
    .await
    .map_err(|_| invalid(format!("{step}: viewer never saw the program's output")))?
}

pub(super) async fn exercise(target: &Target) -> io::Result<()> {
    let a = Client::connect(target.command(&["connect"])).await?;
    refuse_protected_core(
        &a.hello.core.id,
        std::env::var(PROTECTED_CORES_ENV).ok().as_deref(),
    )?;
    let b = Client::connect(target.command(&["connect"])).await?;
    if b.hello.core.id != a.hello.core.id {
        return Err(invalid("viewers reached different cores"));
    }
    if !a.call("terminal.list", json!({"all":true})).await?["terminals"]
        .as_array()
        .is_some_and(Vec::is_empty)
    {
        return Err(invalid("scratch core was not empty"));
    }
    let core = a.hello.core.id.clone();
    let advertised = a.hello.features.contains("automatic_input");
    println!(
        "multiview hc: {} automatic_input advertised={advertised}",
        a.hello.version
    );

    let run = a
        .call(
            "terminal.run",
            json!({"argv":["/bin/cat"],"cwd":"/var/tmp",
                "size":{"cols":B_SIZE.0,"rows":B_SIZE.1},"no_parent":true,"no_notify":true}),
        )
        .await?;
    let address = run["address"]
        .as_str()
        .ok_or_else(|| invalid("run has no address"))?
        .to_owned();
    let pid = record(&a, &address).await?["pid"].clone();
    if pid.as_u64().is_none() {
        return Err(invalid("scratch program has no pid"));
    }

    // B attaches first and keeps the smaller size; A then becomes the last
    // active viewer and controls the size.
    let (_, mut stream_b) = b
        .open(
            "terminal.attach",
            json!({"terminal":address,"cols":B_SIZE.0,"rows":B_SIZE.1}),
        )
        .await?;
    expect_size(&a, &address, B_SIZE, "B attached").await?;
    let (_, mut stream_a) = a
        .open(
            "terminal.attach",
            json!({"terminal":address,"cols":A_SIZE.0,"rows":A_SIZE.1}),
        )
        .await?;
    expect_size(&a, &address, A_SIZE, "A attached last").await?;
    if stream_a.automatic_input != stream_b.automatic_input {
        return Err(invalid("viewers negotiated different input framing"));
    }
    let separate_frame = stream_b.automatic_input;

    // The question this probe exists for: B's terminal answers a query while A
    // is in control. With AUTOMATIC_INPUT the answer is not viewer activity.
    // Without it Native falls back to DATA, which the source reads as typing.
    for round in 1..=6 {
        stream_b
            .input(Bytes::from_static(AUTOMATIC_REPLY), true)
            .await?;
        let step = format!("automatic reply {round} from B");
        if separate_frame {
            expect_size_holds(&a, &address, A_SIZE, &step).await?;
        } else {
            expect_size(&a, &address, B_SIZE, &step).await?;
            // Give control back to A so every round starts from the same state.
            stream_a.focus(true).await?;
            expect_size(&a, &address, A_SIZE, "A refocused").await?;
        }
    }
    println!(
        "multiview automatic replies: framing={} size_stolen_from_controlling_viewer={}",
        if separate_frame {
            "AUTOMATIC_INPUT"
        } else {
            "DATA fallback"
        },
        !separate_frame
    );

    // Manual input is activity on every core: B takes the size, and both
    // viewers see the program's echo.
    stream_b
        .input(Bytes::from_static(b"HC_MULTIVIEW_MANUAL\r"), false)
        .await?;
    expect_size(&a, &address, B_SIZE, "B typed").await?;
    drain_until(&mut stream_a, b"HC_MULTIVIEW_MANUAL", "A after B typed").await?;
    drain_until(&mut stream_b, b"HC_MULTIVIEW_MANUAL", "B after B typed").await?;

    // Focus and resize are activity too; losing focus is not.
    stream_a.focus(true).await?;
    expect_size(&a, &address, A_SIZE, "A gained focus").await?;
    stream_b.focus(false).await?;
    expect_size_holds(&a, &address, A_SIZE, "B lost focus").await?;
    stream_a.resize(110, 30).await?;
    expect_size(&a, &address, (110, 30), "A resized").await?;

    // Detaching either viewer, and reattaching, never ends or replaces the program.
    drop(stream_a);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_detach = record(&b, &address).await?;
    if after_detach["state"] != "live" || after_detach["pid"] != pid {
        return Err(invalid("detaching a viewer disturbed the program"));
    }
    let (_, stream_a) = a
        .open(
            "terminal.attach",
            json!({"terminal":address,"cols":A_SIZE.0,"rows":A_SIZE.1}),
        )
        .await?;
    expect_size(&a, &address, A_SIZE, "A reattached").await?;
    let reattached = record(&a, &address).await?;
    if reattached["state"] != "live" || reattached["pid"] != pid || a.hello.core.id != core {
        return Err(invalid("reattaching changed the program or its owner"));
    }
    drop(stream_a);
    drop(stream_b);

    a.call("terminal.kill", json!({"terminal":address})).await?;
    timeout(Duration::from_secs(10), async {
        loop {
            if record(&a, &address).await?["state"] == "finished" {
                return Ok::<_, io::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| invalid("scratch program did not finish after an explicit kill"))??;
    println!("multiview: attach order, manual input, focus, resize, detach/reattach and explicit kill verified; pid and owner unchanged");
    Ok(())
}
