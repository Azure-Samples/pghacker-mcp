// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/*
 * What the page says. The examples come from the requirements' use cases and
 * the evaluation (eval/questions.json), each checked against the deployed
 * tools: the quotes, authors, dates, file lines and counts are what they
 * return. Keep them that way when changing them.
 */

export const PRODUCT = {
  name: "pghacker-mcp",
  subtitle: "pgsql-hackers mailing list and code history MCP server",
  summary:
    "pghacker-mcp is a public MCP server for AI agents. It indexes the pgsql-hackers mailing list and PostgreSQL's source with its full commit history, so your agent finds the evidence in seconds instead of searching the archive blindly, and answers with citations you can check.",
  citationNote: "Every answer cites its evidence: Message-IDs with verbatim quotes, and file paths with line numbers.",
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

export const ENDPOINT_FACTS = ["Streamable HTTP", "No sign-in or key", "Read-only", "Rate-limited per client"] as const;

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
    id: "jit",
    kind: "History across releases",
    prompt: "Was JIT compilation enabled by default in PostgreSQL 11? How did that differ in PostgreSQL 12, and why?",
    answer:
      "No. PostgreSQL 11 shipped with jit = off: Tom Lane disabled it on the v11 branch only, on 15 September 2018 (first in 11beta4), because JIT wasn't quite mature enough to ship enabled by default. Master kept it on, so PostgreSQL 12 was the first release with JIT on by default.",
    citations: [
      {
        kind: "commit",
        sha: "0fdadfb0192935e0f8bec7fa5ec9dbb470d6d587",
        branch: "REL_11_STABLE",
        date: "2018-09-15",
        author: "Tom Lane",
        subject: "In v11, disable JIT by default (it's still enabled by default in HEAD).",
      },
      {
        kind: "mail",
        author: "Andres Freund",
        date: "2018-09-14",
        quote: "Disable it by default in v11, leave it enabled in master.",
        messageId: "20180914222657.mw25esrzbcnu6qlu@alap3.anarazel.de",
      },
      {
        kind: "mail",
        author: "Tom Lane",
        date: "2018-09-14",
        quote: "I'd go with #2, personally.",
        messageId: "3949.1536964323@sss.pgh.pa.us",
      },
    ],
    tools: ["search_commits", "get_commit", "get_thread"],
  },
  {
    id: "python",
    kind: "A committer's view",
    prompt:
      "What is Tom Lane's position on requiring a newer Python than distributions package, for the pytest-based tests?",
    answer:
      "He's against it: buildfarm owners won't install a Python their platform doesn't package. He'd rather the pytest framework give up quietly when Python is too old, and sees the minimum version as a tradeoff between ease of coding and how many platforms can run the tests.",
    citations: [
      {
        kind: "mail",
        author: "Tom Lane",
        date: "2026-06-21",
        quote: "they will simply not enable pytest testing",
        messageId: "95014.1782050569@sss.pgh.pa.us",
      },
      {
        kind: "mail",
        author: "Tom Lane",
        date: "2026-06-18",
        quote: "our pytest framework should silently give up if Python is too old",
        messageId: "1408166.1781825714@sss.pgh.pa.us",
      },
    ],
    tools: ["author_perspective"],
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
