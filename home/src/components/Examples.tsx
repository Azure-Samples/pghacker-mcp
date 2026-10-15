import { useState } from "react";
import { Bot, ChevronDown, FileCode2, GitCommitHorizontal, MessageSquareQuote } from "lucide-react";
import { EXAMPLES, PRODUCT, shortMessageId, type Citation, type Example } from "../content.ts";
import { CopyButton } from "./CopyButton.tsx";

export function ToolChips({ tools }: { tools: readonly string[] }) {
  return (
    <ul className="chips" aria-label="Tools">
      {tools.map((tool) => (
        <li key={tool} className="chip">
          {tool}
        </li>
      ))}
    </ul>
  );
}

function CitationList({ citations }: { citations: readonly Citation[] }) {
  if (citations.length === 0) return null;
  return (
    <ul className="cites" aria-label="Citations">
      {citations.map((citation) =>
        citation.kind === "mail" ? (
          <li key={citation.messageId} className="cite">
            <MessageSquareQuote className="cite-icon" aria-hidden="true" />
            <div className="cite-body">
              <q>{citation.quote}</q>
              <span className="cite-meta">
                {citation.author} · <time dateTime={citation.date}>{citation.date}</time> ·{" "}
                <code title={citation.messageId}>{shortMessageId(citation.messageId)}</code>
              </span>
            </div>
          </li>
        ) : citation.kind === "commit" ? (
          <li key={citation.sha} className="cite">
            <GitCommitHorizontal className="cite-icon" aria-hidden="true" />
            <div className="cite-body">
              <span className="cite-subject">{citation.subject}</span>
              <span className="cite-meta">
                {citation.author} · <time dateTime={citation.date}>{citation.date}</time> · {citation.branch} ·{" "}
                <code title={citation.sha}>{citation.sha.slice(0, 10)}</code>
              </span>
            </div>
          </li>
        ) : (
          <li key={citation.location} className="cite">
            <FileCode2 className="cite-icon" aria-hidden="true" />
            <div className="cite-body">
              <code className="cite-excerpt">{citation.excerpt}</code>
              <span className="cite-meta">
                <code>{citation.location}</code> · {citation.ref}
              </span>
            </div>
          </li>
        ),
      )}
    </ul>
  );
}

function Answer({ example }: { example: Example }) {
  return (
    <div className="answer">
      <p className="answer-text">{example.answer}</p>
      {example.sql !== undefined && (
        <pre className="sql">
          <code>{example.sql}</code>
        </pre>
      )}
      <CitationList citations={example.citations} />
    </div>
  );
}

/** The examples: their prompts, which expand to each answer beside its prompt. */
export function ExamplesCard() {
  const [open, setOpen] = useState(false);
  return (
    <section className="card examples" id="examples" aria-labelledby="examples-title">
      <header className="examples-head">
        <span className="icon-tile">
          <Bot aria-hidden="true" />
        </span>
        <div className="examples-title">
          <h2 id="examples-title">Example prompts</h2>
          <p>What an agent connected to {PRODUCT.name} answers, and the evidence it cites.</p>
        </div>
        <button
          type="button"
          className="btn btn-outline btn-sm"
          aria-expanded={open}
          aria-controls="examples-list"
          onClick={() => setOpen(!open)}
        >
          {open ? "Hide the answers" : `Show ${EXAMPLES.length} answers`}
          <ChevronDown className="toggle-chevron" aria-hidden="true" />
        </button>
      </header>
      <div id="examples-list">
        {open ? (
          <ol className="example-list">
            {EXAMPLES.map((example) => (
              <li key={example.id} className="example">
                <div className="example-q">
                  <span className="kicker">{example.kind}</span>
                  <p className="prompt-text">{example.prompt}</p>
                  <div>
                    <CopyButton text={example.prompt} label={`the prompt: ${example.kind}`} variant="outline">
                      Copy prompt
                    </CopyButton>
                  </div>
                  <ToolChips tools={example.tools} />
                </div>
                <div className="example-a">
                  <span className="kicker">What you get</span>
                  <Answer example={example} />
                </div>
              </li>
            ))}
          </ol>
        ) : (
          <ul className="teasers">
            {EXAMPLES.map((example) => (
              <li key={example.id}>
                <span className="kicker">{example.kind}</span>
                <span className="teaser-prompt">{example.prompt}</span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}
