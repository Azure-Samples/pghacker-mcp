// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { useId, useRef, useState, type KeyboardEvent } from "react";
import { Check, ChevronRight, PlugZap } from "lucide-react";
import { CLIENTS, configPrefix } from "../clients.ts";
import { ENDPOINT_FACTS, FIRST_PROMPT } from "../content.ts";
import { CopyButton } from "./CopyButton.tsx";

/**
 * The MCP server's panel: its status and facts, then three steps: copy the
 * endpoint, add it to your agent, ask.
 */
export function ConnectPanel({ url }: { url: string }) {
  return (
    <section className="card connect" id="connect" aria-labelledby="connect-title">
      <header className="connect-head">
        <span className="icon-tile icon-tile-success">
          <PlugZap aria-hidden="true" />
        </span>
        <h2 id="connect-title">MCP server</h2>
        <span className="pill pill-success">
          <span className="dot" aria-hidden="true" />
          Available
        </span>
        <ul className="facts" aria-label="About the endpoint">
          {ENDPOINT_FACTS.map((fact) => (
            <li key={fact}>
              <Check aria-hidden="true" />
              {fact}
            </li>
          ))}
        </ul>
      </header>
      <ol className="steps">
        <li className="step">
          <span className="step-num" aria-hidden="true">
            1
          </span>
          <div className="step-body">
            <h3>Copy the endpoint</h3>
            <div className="endpoint">
              <code data-mcp-url>{url}</code>
              <CopyButton text={url} label="the MCP URL" variant="primary">
                Copy URL
              </CopyButton>
            </div>
          </div>
        </li>
        <li className="step">
          <span className="step-num" aria-hidden="true">
            2
          </span>
          <div className="step-body">
            <h3>Add it to your agent</h3>
            <ClientSteps url={url} />
          </div>
        </li>
        <li className="step">
          <span className="step-num" aria-hidden="true">
            3
          </span>
          <div className="step-body">
            <h3>Ask, and check the citations</h3>
            <div className="try">
              <p>“{FIRST_PROMPT}”</p>
              <CopyButton text={FIRST_PROMPT} label="the first prompt" />
            </div>
          </div>
        </li>
      </ol>
    </section>
  );
}

/** Each agent's steps, by tab: the first thing to do, a command, and its configuration file, collapsed. */
function ClientSteps({ url }: { url: string }) {
  const [selected, setSelected] = useState(0);
  const [open, setOpen] = useState(false);
  const tabs = useRef<(HTMLButtonElement | null)[]>([]);
  const id = useId();
  const client = CLIENTS[selected];
  const command = client.command?.(url);
  const config = client.config;

  function select(index: number) {
    setSelected(index);
    setOpen(false);
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const moves: Record<string, number> = {
      ArrowRight: (selected + 1) % CLIENTS.length,
      ArrowLeft: (selected - 1 + CLIENTS.length) % CLIENTS.length,
      Home: 0,
      End: CLIENTS.length - 1,
    };
    const next = moves[event.key];
    if (next === undefined) return;
    event.preventDefault();
    select(next);
    tabs.current[next]?.focus();
  }

  return (
    <div className="client-setup">
      <div className="tabs" role="tablist" aria-label="Your agent" onKeyDown={onKeyDown}>
        {CLIENTS.map((candidate, index) => (
          <button
            key={candidate.id}
            ref={(element) => {
              tabs.current[index] = element;
            }}
            type="button"
            role="tab"
            id={`${id}-tab-${candidate.id}`}
            className="tab"
            aria-selected={index === selected}
            aria-controls={`${id}-panel`}
            tabIndex={index === selected ? 0 : -1}
            onClick={() => select(index)}
          >
            {candidate.label}
          </button>
        ))}
      </div>
      <div className="client-panel" role="tabpanel" id={`${id}-panel`} aria-labelledby={`${id}-tab-${client.id}`}>
        <p className="client-action">{client.action}</p>
        {command !== undefined && (
          <div className="snippet">
            <pre>
              <code>
                <span className="prompt-sign" aria-hidden="true">
                  ${" "}
                </span>
                {command}
              </code>
            </pre>
            <CopyButton text={command} label={`the ${client.label} command`} />
          </div>
        )}
        {config !== undefined && (
          <div className="config">
            <button
              type="button"
              className="disclosure"
              aria-expanded={open}
              aria-controls={`${id}-config`}
              onClick={() => setOpen(!open)}
            >
              <ChevronRight className="disclosure-chevron" aria-hidden="true" />
              {configPrefix(config)} <code>{config.file}</code>
            </button>
            <div className="snippet" id={`${id}-config`} hidden={!open}>
              <pre>
                <code>{config.text(url)}</code>
              </pre>
              <CopyButton text={config.text(url)} label={config.file} />
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
