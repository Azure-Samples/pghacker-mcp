// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * What the page says. The examples come from the requirements' use cases,
 * the evaluation (eval/questions.json), exploratory testing and a run of the
 * review-as-author skill, each checked against the deployed tools: the
 * quotes, authors, dates, file lines and counts are what they return. Keep
 * them that way when changing them.
 */

export const PRODUCT = {
  name: "pghacker-mcp",
  subtitle: "pgsql-hackers mailing list and code history MCP server",
  summary:
    "pghacker-mcp is a public MCP server for AI agents. It indexes the pgsql-hackers mailing list and PostgreSQL's source with its full commit history, so your agent finds the evidence in seconds instead of searching the archive blindly, and answers with citations you can check.",
  citationNote: "Every answer cites its evidence: Message-IDs with verbatim quotes, and file paths with line numbers.",
} as const;

/**
 * What the service runs on, said beside HorizonDB's logo below the connect
 * panel. The logo is Azure HorizonDB's product icon, loaded from Microsoft's
 * CDN, where azure.microsoft.com/products/horizondb gets it; the page's CSP
 * allows that host (scripts/lib/edge.mjs).
 */
export const POWERED_BY = {
  prefix: "Powered by",
  name: "HorizonDB",
  logo: "https://cdn-dynmedia-1.microsoft.com/is/content/microsoftcorp/1374850-Icon-Hero-HorizonDB-24x24",
} as const;

export type CapabilityIcon = "history" | "people" | "code" | "sql";

export interface Capability {
  readonly id: CapabilityIcon;
  readonly title: string;
  readonly text: string;
  readonly tools: readonly string[];
}

export const CAPABILITIES: readonly Capability[] = [
  {
    id: "history",
    title: "History and decisions",
    text: "Follow a feature from proposal to commit, revert or release, with the options that were ruled out and why.",
    tools: ["search_extracts", "search_threads", "get_thread"],
  },
  {
    id: "people",
    title: "Committers' perspectives",
    text: "An author's cited positions on a topic, to see how a committer would review your patch.",
    tools: ["author_perspective", "get_author"],
  },
  {
    id: "code",
    title: "Code and its history",
    text: "Search and read any release or commit, blame a line, and find the commits, patches and threads behind it.",
    tools: ["search", "grep", "read_file", "blame", "search_commits", "search_patches"],
  },
  {
    id: "sql",
    title: "SQL and the graph",
    text: "Read-only SQL over the corpus, and Cypher over the graph of authors, threads, features and files.",
    tools: ["query", "get_ontology"],
  },
];

export const ENDPOINT_FACTS = ["Streamable HTTP", "No sign-in"] as const;

export const FIRST_PROMPT = "Use pghacker to find out what happened to SQL/PGQ in PostgreSQL 19.";

export interface MailCitation {
  readonly kind: "mail";
  readonly author: string;
  /** When the message was sent, YYYY-MM-DD. */
  readonly date: string;
  readonly quote: string;
  readonly messageId: string;
}

export interface CodeCitation {
  readonly kind: "code";
  /** path:line. */
  readonly location: string;
  /** The branch or release it was read at. */
  readonly ref: string;
  readonly excerpt: string;
}

export interface CommitCitation {
  readonly kind: "commit";
  readonly sha: string;
  /** The branch it was committed to. */
  readonly branch: string;
  readonly date: string;
  readonly author: string;
  readonly subject: string;
}

export type Citation = MailCitation | CodeCitation | CommitCitation;

export interface Example {
  readonly id: string;
  readonly kind: string;
  readonly prompt: string;
  readonly answer: string;
  readonly citations: readonly Citation[];
  /** The statement the agent ran, for an answer from SQL. */
  readonly sql?: string;
  readonly tools: readonly string[];
}

export const EXAMPLES: readonly Example[] = [
  {
    id: "pgq",
    kind: "Feature history",
    prompt: "What happened to SQL/PGQ (property graph queries) in PostgreSQL 19?",
    answer:
      "It was committed for 19 in March 2026, then reverted from REL_19_STABLE and master in September 2026. The release management team found unresolved design and behavior questions, such as metadata left behind by CASCADE drops and pg_dump's dependency handling, and too little time to settle them before the release.",
    citations: [
      {
        kind: "mail",
        author: "Peter Eisentraut",
        date: "2026-03-16",
        quote: "I have committed this, including the 0004 patch.",
        messageId: "bcf58f6e-d0bd-49f8-b074-e3ee69ef6567@eisentraut.org",
      },
      {
        kind: "mail",
        author: "Melanie Plageman",
        date: "2026-09-02",
        quote: "we feel it would be better to revert this in 19",
        messageId: "CAAKRu_bEtjWYWhYxSo0o_t3DaZYRQd5PkC0abA0g9Mp4+ovH0w@mail.gmail.com",
      },
      {
        kind: "mail",
        author: "Peter Eisentraut",
        date: "2026-09-03",
        quote: "Ok, let's do it.",
        messageId: "96b179ff-0be2-4467-8fd7-f3afae8d360b@eisentraut.org",
      },
    ],
    tools: ["search_extracts", "get_thread"],
  },
  {
    id: "ddl",
    kind: "Review as an author",
    prompt: 'Review the May 2022 "Support logical replication of DDLs" patch set as Tom Lane would have then.',
    answer:
      "He'd likely push back on adding it to core: reviewing an earlier version of this deparsing approach in 2014, he called it \"large and rickety\", and in 2012 he warned that decompiling parse trees would be \"a permanent drag on every type of SQL feature addition\". Posted on 10 May 2022, this review would have saved the community more than ten months: Tom Lane gave the same feedback only on 26 March 2023, 80 patch versions later, when the patch set had \"ballooned to over 2MB\" and he didn't \"quite believe the premise\". It was withdrawn in 2024.",
    citations: [
      {
        kind: "mail",
        author: "Tom Lane",
        date: "2014-11-08",
        quote: "It looks large and rickety (ie full of opportunities for bugs) and I would never trust data I cared about to it.",
        messageId: "21908.1415466461@sss.pgh.pa.us",
      },
      {
        kind: "mail",
        author: "Tom Lane",
        date: "2012-10-05",
        quote:
          "Adding a requirement to be able to decompile raw parse trees will be a permanent drag on every type of SQL feature addition.",
        messageId: "10767.1349453701@sss.pgh.pa.us",
      },
      {
        kind: "mail",
        author: "Tom Lane",
        date: "2023-03-26",
        quote: "I don't think that I quite believe the premise in the first place.",
        messageId: "3032112.1679865718@sss.pgh.pa.us",
      },
    ],
    tools: ["author_perspective", "search_threads", "get_thread"],
  },
  {
    id: "fsync",
    kind: "Design decision",
    prompt: "Can retrying a failed disk flush actually cause data loss?",
    answer:
      "Yes. In March 2018 Craig Ringer showed that on Linux a failed fsync() can clear its own error, so a retried checkpoint \"succeeded\" though the writes never reached disk, leaving WAL as the only copy. After more than 250 messages in two threads, PostgreSQL chose in November 2018 to stop rather than retry, back to 9.4: a data-file fsync() failure now PANICs, and crash recovery replays the WAL. The data_sync_retry setting, off by default, allows retrying on systems that keep the unwritten data.",
    citations: [
      {
        kind: "mail",
        author: "Craig Ringer",
        date: "2018-03-28",
        quote: "Retrying fsync() is not OK at least on Linux.",
        messageId: "CAMsr+YHh+5Oq4xziwwoEfhoTZgr07vdGG+hu=1adXx59aTeaoQ@mail.gmail.com",
      },
      {
        kind: "mail",
        author: "Andres Freund",
        date: "2018-04-09",
        quote: "We should just deal with this by crash-recovery.",
        messageId: "20180409200420.2shb4xygozkl3zr2@alap3.anarazel.de",
      },
      {
        kind: "commit",
        sha: "9ccdd7f66e3324d2b6d3dec282cfa9ff084083f1",
        branch: "master",
        date: "2018-11-19",
        author: "Thomas Munro",
        subject: "PANIC on fsync() failure.",
      },
    ],
    tools: ["search_threads", "get_thread", "get_commit"],
  },
  {
    id: "plan",
    kind: "Code and history",
    prompt: "Does PostgreSQL 19 support the JSON_TABLE PLAN clause? Check both the source code and the mailing list.",
    answer:
      "No. The json_table rule in REL_19_STABLE's gram.y has no PLAN clause: it takes the context item, path, PASSING, COLUMNS and ON ERROR. PLAN was left out when JSON_TABLE was committed in April 2024, and was committed only in July 2026, by Alexander Korotkov, which isn't in 19.",
    citations: [
      {
        kind: "code",
        location: "src/backend/parser/gram.y:14573",
        ref: "REL_19_STABLE",
        excerpt:
          "json_table: JSON_TABLE '(' json_value_expr ',' a_expr json_table_path_name_opt json_passing_clause_opt COLUMNS '(' … ')' json_on_error_clause_opt ')'",
      },
      {
        kind: "mail",
        author: "Amit Langote",
        date: "2024-04-08",
        quote: "Let's work on the remaining PLAN clause with a new entry in the next CF",
        messageId: "CA+HiwqFosrEbh_9KPX5hsd+n80F1m6CSVgNLYtvhhO1fsrBk6w@mail.gmail.com",
      },
      {
        kind: "mail",
        author: "Alexander Korotkov",
        date: "2026-07-07",
        quote: "I'm going to push this if no objections.",
        messageId: "CAPpHfdtJB2APBjD7Npa58Cg4P0KqFjNKq_mT4tpNWN2y+5zKoA@mail.gmail.com",
      },
    ],
    tools: ["search", "read_file", "search_threads"],
  },
  {
    id: "sql",
    kind: "SQL",
    prompt: "Who were the three most active posters on pgsql-hackers in June and July 2026?",
    answer: "Michael Paquier (258 messages), Tom Lane (223) and Chao Li (186).",
    sql: [
      "SELECT a.display_name, count(*) AS messages",
      "FROM pghacker.messages m",
      "JOIN pghacker.authors a ON a.id = m.author_id",
      "WHERE m.list = 'pgsql-hackers'",
      "  AND m.sent_at >= '2026-06-01' AND m.sent_at < '2026-08-01'",
      "GROUP BY 1 ORDER BY 2 DESC LIMIT 3;",
    ].join("\n"),
    citations: [],
    tools: ["query"],
  },
];

/** A long Message-ID shortened in the middle, for display; the full one goes in a title. */
export function shortMessageId(id: string, keep = 18): string {
  const at = id.lastIndexOf("@");
  const local = at > 0 ? id.slice(0, at) : id;
  const domain = at > 0 ? id.slice(at) : "";
  return local.length > keep + 1 ? `${local.slice(0, keep)}…${domain}` : id;
}
