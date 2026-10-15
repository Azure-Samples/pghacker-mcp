import { PRODUCT } from "../content.ts";

/** The mark: a speech bubble with a quotation, for cited answers, on the brand colour. */
export function BrandMark({ size = 32 }: { size?: number }) {
  return (
    <svg className="brand-mark" width={size} height={size} viewBox="0 0 32 32" aria-hidden="true" focusable="false">
      <rect width="32" height="32" rx="9" fill="currentColor" />
      <path d="M9.5 9h13a3 3 0 0 1 3 3v7a3 3 0 0 1-3 3H16l-5 4v-4H9.5a3 3 0 0 1-3-3v-7a3 3 0 0 1 3-3z" fill="white" />
      <path
        d="M11.6 12.8h3v3c0 1.7-.8 2.8-2.4 3.4l-.5-1c.8-.4 1.2-1 1.2-1.9h-1.3zm5.8 0h3v3c0 1.7-.8 2.8-2.4 3.4l-.5-1c.8-.4 1.2-1 1.2-1.9h-1.3z"
        fill="currentColor"
      />
    </svg>
  );
}

export function TopBar() {
  return (
    <header className="topbar">
      <div className="topbar-inner">
        <a className="brand" href="/">
          <BrandMark />
          <span className="brand-copy">
            <span className="brand-name">{PRODUCT.name}</span>
            <span className="brand-sub">{PRODUCT.subtitle}</span>
          </span>
        </a>
        <nav className="topbar-nav" aria-label="On this page">
          <a href="#connect">Connect</a>
          <a href="#examples">Examples</a>
        </nav>
      </div>
    </header>
  );
}

export function Footer() {
  return (
    <footer className="footer">
      <p>
        {PRODUCT.name} is built on <a href="https://github.com/microsoft/PilotSwarm">PilotSwarm</a>. Sources: the
        pgsql-hackers archive at postgresql.org and PostgreSQL's git repository. Not affiliated with the PostgreSQL
        Global Development Group.
      </p>
    </footer>
  );
}
