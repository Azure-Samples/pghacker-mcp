import { PRODUCT } from "./content.ts";
import { Capabilities } from "./components/Capabilities.tsx";
import { ConnectPanel } from "./components/Connect.tsx";
import { ExamplesCard } from "./components/Examples.tsx";
import { Footer, TopBar } from "./components/Frame.tsx";

/** The home page, for the MCP endpoint at `url`. */
export function App({ url }: { url: string }) {
  return (
    <>
      <a className="skip-link" href="#main">
        Skip to the page
      </a>
      <TopBar />
      <main id="main" tabIndex={-1}>
        <section className="hero" aria-labelledby="title">
          <span className="pill pill-primary">
            <span className="dot" aria-hidden="true" />
            Public MCP server · no sign-in
          </span>
          <h1 id="title">
            PostgreSQL's design history and code, <span className="accent">with citations</span>, for your AI agent
          </h1>
          <p className="lede">{PRODUCT.summary}</p>
          <ConnectPanel url={url} />
        </section>
        <Capabilities />
        <div className="section section-examples">
          <ExamplesCard />
        </div>
      </main>
      <Footer />
    </>
  );
}
