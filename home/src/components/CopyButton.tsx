import { useEffect, useRef, useState, type ReactNode } from "react";
import { Check, Copy, TriangleAlert } from "lucide-react";

type Result = { readonly kind: "idle" } | { readonly kind: "copied" } | { readonly kind: "failed"; readonly reason: string };

/**
 * A button that copies `text` and says whether it did: a check mark for a
 * moment, or why it couldn't, for everyone, screen readers too.
 */
export function CopyButton({
  text,
  label,
  variant = "icon",
  children,
}: {
  text: string;
  /** What it copies, such as "the MCP URL": the icon button's name, a text button's tooltip, and the status. */
  label: string;
  variant?: "icon" | "primary" | "outline";
  children?: ReactNode;
}) {
  const [result, setResult] = useState<Result>({ kind: "idle" });
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  async function copy() {
    window.clearTimeout(timer.current);
    try {
      await navigator.clipboard.writeText(text);
      setResult({ kind: "copied" });
    } catch (error) {
      setResult({ kind: "failed", reason: error instanceof Error ? error.message : "the clipboard isn't available" });
    }
    timer.current = window.setTimeout(() => setResult({ kind: "idle" }), 2000);
  }

  const icon =
    result.kind === "copied" ? (
      <Check className="icon-success" aria-hidden="true" />
    ) : result.kind === "failed" ? (
      <TriangleAlert className="icon-failed" aria-hidden="true" />
    ) : (
      <Copy aria-hidden="true" />
    );
  const status = (
    <span className={result.kind === "failed" ? "copy-failed" : "sr-only"} role="status">
      {result.kind === "copied" ? `Copied ${label}` : result.kind === "failed" ? `Couldn't copy: ${result.reason}` : ""}
    </span>
  );

  if (variant === "icon") {
    return (
      <span className="copy">
        <button type="button" className="icon-btn" aria-label={`Copy ${label}`} title={`Copy ${label}`} onClick={copy}>
          {icon}
        </button>
        {status}
      </span>
    );
  }
  return (
    <span className="copy">
      <button
        type="button"
        className={variant === "primary" ? "btn btn-primary" : "btn btn-outline btn-xs"}
        title={`Copy ${label}`}
        onClick={copy}
      >
        {icon}
        {result.kind === "copied" ? "Copied" : children}
      </button>
      {status}
    </span>
  );
}
