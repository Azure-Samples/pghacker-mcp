// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { portalEnvironment, readLocalConfig, serviceUrl } from "./lib/config.mjs";
import { callTool, mcpRequest } from "./lib/services.mjs";
import { appAgentNames, modelProvidersPath } from "./lib/worker.mjs";
import {
  calledTool, checkCommits, checkReview, citedCommits, lastJsonBlock, parseCsv, quoteIn, resultsTable, REVIEW_SKILL_TOOL,
  reviewPrompt, reviewTable, toolsCalled, verifySkill,
} from "./lib/evalcheck.mjs";

// The evaluation (npm start must be running):
//   npm run eval -- [--questions eval/questions.json] [--only q01,q02] [--concurrency 3] [--results eval/RESULTS.md]
//   npm run eval -- --questions eval/reviews.json --skill skills/review-as-author/SKILL.md --results eval/RESULTS-reviews.md
// To evaluate a deployment, start the app with PGHACKER_AGENT_MCP_URL set to
// its public /mcp, so that the agents use it, and pass the same URL as
// --mcp-url, so that the citations are checked against it.
// Each question goes to a pghacker-eval session, which may use only
// pghacker-mcp. The script checks every citation (the Message-ID exists and
// the quote is verbatim) and every cited commit (a full SHA that the
// history's code_commits holds), and a judge session grades the answer
// against the gold answer. A question passes when it is judged correct, its
// citations are valid when it has gold citations, and, when it has gold
// commits, its commits are valid and include one of them. Full results go to
// .runtime/eval; the summary to eval/RESULTS.md.
//
// Review questions (kind "review", eval/reviews.json) ask for a patch's
// review as its reviewer would have given it when it was posted; their gold
// answers are the points that reviewer actually raised. With
// --skill skills/review-as-author/SKILL.md (or the skill's skill:// URI,
// read from the server and checked against its digests), the prompt carries
// the skill; without it, the same prompt goes without, and the agent, which
// declares no skills extension, can load the skill on its own with
// load_skill_review_as_author. Each result says which tools the session
// called, and a review whether it loaded the skill so. A review
// passes when it is judged to raise the reviewer's main point, its citations
// are valid, at least two quote the reviewer's own words, and none was sent
// on or after the patch's date or is the actual review.
const { values: options } = parseArgs({
  options: {
    questions: { type: "string", default: "eval/questions.json" },
    only: { type: "string", default: "" },
    model: { type: "string", default: "github-copilot:gpt-6-luna" },
    "judge-model": { type: "string", default: "github-copilot:gpt-6-luna" },
    concurrency: { type: "string", default: "3" },
    "timeout-minutes": { type: "string", default: "20" },
    "mcp-url": { type: "string", default: "" },
    results: { type: "string", default: "eval/RESULTS.md" },
    skill: { type: "string", default: "" },
  },
});

const config = readLocalConfig();
const mcp = options["mcp-url"] || serviceUrl(config.mcpPort);
const timeout = Number(options["timeout-minutes"]) * 60_000;
/** A review needs at least this many citations in the reviewer's own words. */
const MIN_OWN_CITATIONS = 2;

const sqlText = (value) => `'${String(value).replace(/'/g, "''")}'`;

/** Messages by Message-ID: body, own words (body_clean), thread, author and date. */
async function messages(ids) {
  if (!ids.length) return new Map();
  const csv = await callTool(mcp, "query", {
    sql: `SELECT json_agg(json_build_object('id', m.message_id, 'thread', m.thread_id, 'body', m.body,
                 'body_clean', m.body_clean, 'author', a.display_name, 'sent_at', m.sent_at))
          FROM messages m LEFT JOIN authors a ON a.id = m.author_id
          WHERE m.message_id IN (${ids.map(sqlText).join(", ")})`,
    max_rows: 10,
  });
  const rows = parseCsv(String(csv));
  const cell = rows[1]?.[0];
  const list = cell && cell !== "None" ? JSON.parse(cell) : [];
  return new Map(list.map((m) => [m.id, m]));
}

/** A review question's patch: its subject and diff, as posted. */
async function postedPatch(messageId) {
  const csv = await callTool(mcp, "query", {
    sql: `SELECT json_build_object('subject', m.subject, 'diff',
                 (SELECT string_agg(p.content, E'\\n' ORDER BY p.part, p.id) FROM patches p WHERE p.message_id = m.id))
          FROM messages m WHERE m.message_id = ${sqlText(messageId)}`,
    max_rows: 1,
  });
  const cell = parseCsv(String(csv))[1]?.[0];
  const patch = cell && cell !== "None" ? JSON.parse(cell) : null;
  if (!patch?.diff) throw new Error(`no patch in ${messageId}`);
  return patch;
}

/**
 * The skill's text: a SKILL.md in the repository, or a skill:// URI read
 * from the server as a host would, checked against its entry's digests.
 */
async function loadSkill(source) {
  if (!source) return "";
  if (!source.startsWith("skill://")) return readFileSync(path.resolve(config.root, source), "utf8");
  const { skill } = await mcpRequest(mcp, "skills/get", { uri: source });
  const texts = new Map();
  for (const { uri } of skill.resources) {
    const { contents } = await mcpRequest(mcp, "resources/read", { uri });
    texts.set(uri, contents[0].text);
  }
  const problems = verifySkill(skill, texts);
  if (problems.length) throw new Error(`${source} doesn't match its entry: ${problems.join("; ")}`);
  return texts.get(source);
}

/** The SHAs among `shas` that the commit history holds. */
async function knownCommits(shas) {
  if (!shas.length) return [];
  const csv = await callTool(mcp, "query", {
    sql: `SELECT sha FROM code_commits WHERE sha IN (${shas.map(sqlText).join(", ")})`,
    max_rows: 1000,
  });
  return parseCsv(String(csv)).slice(1).map((row) => row[0]);
}

const JUDGE = `You grade answers about PostgreSQL's history and code against a gold answer written by an expert.
The gold answer states the key facts first; anything after "Supporting detail:" is context. An answer is
correct when it gives the key facts and contradicts none of the gold answer's facts, supporting detail
included. It may phrase them differently and leave out supporting detail. Details the gold answer doesn't
mention count against it only if they contradict the gold answer. Reply with only a JSON object:
{"correct": true|false, "score": 0.0-1.0, "missing": "key facts missing or wrong", "reason": "one sentence"}`;

const REVIEW_JUDGE = `You grade a predicted code review against the review its reviewer actually gave. The gold
answer lists the reviewer's actual key points first; anything after "Supporting detail:" is context. The
prediction is correct when it raises, in substance, the reviewer's first key point or at least half of the key
points, and attributes to the reviewer nothing that contradicts the gold answer. Wording may differ, and points
the gold answer doesn't mention don't count against it unless they contradict it. The score is the share of key
points the prediction raises. Reply with only a JSON object:
{"correct": true|false, "score": 0.0-1.0, "missing": "key points not raised", "reason": "one sentence"}`;

async function judge(client, question, answer) {
  const review = question.kind === "review";
  const session = await client.createSession({ model: options["judge-model"], systemMessage: review ? REVIEW_JUDGE : JUDGE });
  const asked = review
    ? `A review of the patch "${question.subject}" as ${question.reviewer} would have given it on ${question.as_of}.`
    : question.question;
  const reply = await session.sendAndWait(
    `Question: ${asked}\n\nGold answer: ${question.gold_answer}\n\n${review ? "Prediction" : "Answer"} to grade: ${answer}\n\nReply with the JSON object only.`,
    timeout,
  );
  const match = String(reply ?? "").match(/\{[\s\S]*\}/);
  try {
    return match ? JSON.parse(match[0]) : { correct: false, score: 0, reason: "the judge did not reply with JSON" };
  } catch {
    return { correct: false, score: 0, reason: "the judge's JSON did not parse" };
  }
}

/** The most of a review the judge reads, in characters. */
const REVIEW_CHARS = 12_000;

/**
 * The tools a session called, from its latest events, which a page of 1,000
 * holds for one question; for a review, whether it loaded the review skill
 * with its tool.
 */
async function called(session, review) {
  let tools = [];
  try {
    tools = toolsCalled(await session.getMessages(1000));
  } catch {
    // Without its events, the result says no tools.
  }
  return review ? { tools, loaded_skill: calledTool(tools, REVIEW_SKILL_TOOL) } : { tools };
}

/** What the agent is asked: the question, or a review question's prompt with the patch. */
async function prompt(question) {
  if (question.kind !== "review") return question.question;
  const patch = await postedPatch(question.patch_message_id);
  return reviewPrompt({
    reviewer: question.reviewer,
    asOf: question.as_of,
    subject: question.subject ?? patch.subject,
    diff: patch.diff,
    skill,
  });
}

async function evaluate(client, question) {
  const started = Date.now();
  const review = question.kind === "review";
  const asked = review ? { reviewer: question.reviewer, as_of: question.as_of, subject: question.subject } : {};
  const session = await client.createSessionForAgent("pghacker-eval", { model: options.model, title: `pghacker eval ${question.id}` });
  let reply;
  try {
    reply = await session.sendAndWait(await prompt(question), timeout);
  } catch (error) {
    return { id: question.id, kind: question.kind, question: question.question, ...asked, ...(await called(session, review)), pass: false, error: error.message, session: session.sessionId };
  }
  const tools = await called(session, review);
  const parsed = lastJsonBlock(reply);
  if (!parsed) {
    return { id: question.id, kind: question.kind, question: question.question, ...asked, ...tools, pass: false, error: "no JSON answer block", reply, session: session.sessionId };
  }
  const citations = Array.isArray(parsed.citations) ? parsed.citations : [];
  const goldIds = (question.gold_citations ?? []).map((c) => c.message_id);
  const found = await messages([...new Set([...citations.map((c) => c.message_id), ...goldIds])]);
  const checked = citations.map((c) => {
    const message = found.get(c.message_id);
    return { ...c, exists: Boolean(message), verbatim: Boolean(message) && quoteIn(message.body, c.quote ?? "") };
  });
  const goldThreads = new Set(goldIds.map((id) => found.get(id)?.thread).filter(Boolean));
  const citationsValid = checked.length > 0 && checked.every((c) => c.exists && c.verbatim);
  const onGoldThread = checked.some((c) => goldThreads.has(found.get(c.message_id)?.thread));
  const goldCommits = question.gold_commits ?? [];
  const cited = citedCommits(parsed.commits);
  let commits;
  let commitsError;
  try {
    commits = checkCommits(parsed.commits, await knownCommits(cited.shas), goldCommits);
  } catch (error) {
    commitsError = error.message;
    commits = { ...checkCommits(parsed.commits, [], goldCommits), valid: false };
  }
  const needsCommits = goldCommits.length > 0;
  const citesCommits = cited.shas.length + cited.malformed.length > 0;
  const commitsValid = needsCommits ? commits.commits.length > 0 && commits.valid : citesCommits ? commits.valid : null;
  // A review is graded as written, its prose before the JSON block.
  const graded = review ? String(reply).replace(/```json[\s\S]*$/, "").trim().slice(0, REVIEW_CHARS) : parsed.answer ?? "";
  const grade = await judge(client, question, graded || (parsed.answer ?? ""));
  const needsCitations = goldIds.length > 0;
  if (review) {
    const { own, leaked } = checkReview(checked, found, { reviewer: question.reviewer, asOf: question.as_of, goldIds });
    return {
      id: question.id,
      kind: question.kind,
      ...asked,
      answer: parsed.answer,
      correct: Boolean(grade.correct),
      score: grade.score,
      judge: grade,
      citations: checked,
      citations_valid: citationsValid,
      own_citations: own,
      leaked,
      ...tools,
      pass: Boolean(grade.correct) && citationsValid && own >= MIN_OWN_CITATIONS && leaked.length === 0,
      seconds: Math.round((Date.now() - started) / 1000),
      session: session.sessionId,
      reply,
    };
  }
  return {
    id: question.id,
    kind: question.kind,
    question: question.question,
    answer: parsed.answer,
    correct: Boolean(grade.correct),
    score: grade.score,
    judge: grade,
    citations: checked,
    citations_valid: needsCitations ? citationsValid : checked.every((c) => c.exists && c.verbatim),
    on_gold_thread: onGoldThread,
    files: parsed.files ?? [],
    commits: commits.commits,
    ...(commits.malformed.length ? { commits_malformed: commits.malformed } : {}),
    ...(commits.unknown.length ? { commits_unknown: commits.unknown } : {}),
    ...(commitsError ? { commits_error: commitsError } : {}),
    commits_valid: commitsValid,
    on_gold_commit: needsCommits ? commits.onGold : null,
    ...tools,
    pass: Boolean(grade.correct) && (needsCitations ? citationsValid : true)
      && (needsCommits ? commitsValid && commits.onGold : true),
    seconds: Math.round((Date.now() - started) / 1000),
    session: session.sessionId,
  };
}

const questions = JSON.parse(readFileSync(options.questions, "utf8"))
  .filter((q) => !options.only || options.only.split(",").includes(q.id));
// The skill, read once: review questions carry it.
const skill = await loadSkill(options.skill);
// The client checks that the provider's credential resolves, as the worker does.
const token = portalEnvironment(config).GITHUB_TOKEN;
if (token) process.env.GITHUB_TOKEN = token;
const { PilotSwarmClient } = await import("pilotswarm-sdk");
const client = new PilotSwarmClient({
  store: config.databaseUrl,
  modelProvidersPath: modelProvidersPath(config),
  allowedAgentNames: await appAgentNames(config),
});
await client.start();
const results = [];
try {
  const queue = [...questions];
  const workers = Array.from({ length: Math.max(1, Number(options.concurrency)) }, async () => {
    while (queue.length) {
      const question = queue.shift();
      const result = await evaluate(client, question);
      results.push(result);
      const checks = result.kind === "review"
        ? ` correct=${result.correct} score=${result.score} citations_valid=${result.citations_valid}` +
          ` own=${result.own_citations} leaked=${result.leaked.length} loaded_skill=${result.loaded_skill}`
        : ` correct=${result.correct} citations_valid=${result.citations_valid}` +
          `${result.on_gold_commit === null ? "" : ` commits_valid=${result.commits_valid} gold_commit=${result.on_gold_commit}`}`;
      console.log(`[eval] ${result.id}: ${result.pass ? "PASS" : "FAIL"}${result.error ? ` (${result.error})` : checks}`);
    }
  });
  await Promise.all(workers);
} finally {
  await client.stop();
}

results.sort((a, b) => a.id.localeCompare(b.id));
const passed = results.filter((r) => r.pass).length;
const withCommits = new Set(questions.filter((q) => (q.gold_commits ?? []).length).map((q) => q.id));
const tally = (keep) => {
  const group = results.filter((r) => keep(withCommits.has(r.id)));
  return `${group.filter((r) => r.pass).length} of ${group.length}`;
};
const reviews = results.length > 0 && results.every((r) => r.kind === "review");
const scores = results.map((r) => r.score).filter((score) => typeof score === "number");
const meanScore = scores.length ? scores.reduce((sum, score) => sum + score, 0) / results.length : 0;
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const dir = path.join(config.runtimeDir, "eval");
mkdirSync(dir, { recursive: true });
writeFileSync(path.join(dir, `eval-${stamp}.json`), JSON.stringify({ options, results }, null, 2));
const lines = reviews
  ? [
    "# pghacker-mcp evaluation: reviews as their authors would give them",
    "",
    `${passed} of ${results.length} reviews passed (judged to raise the reviewer's main point, with valid verbatim citations, at least ${MIN_OWN_CITATIONS} in the reviewer's own words, and none sent on or after the patch's date or from the actual review).`,
    `Mean score, the share of the reviewer's actual points raised: ${meanScore.toFixed(2)}.`,
    `Skill: ${options.skill ? `\`${options.skill}\` in the prompt` : `none in the prompt; ${results.filter((r) => r.loaded_skill).length} of ${results.length} reviews loaded it on their own, with \`${REVIEW_SKILL_TOOL}\``}.`,
  ]
  : [
    "# pghacker-mcp evaluation",
    "",
    `${passed} of ${results.length} questions passed (judged correct, with valid verbatim citations and, for questions with gold commits, a valid cited gold commit).`,
    ...(withCommits.size && withCommits.size < results.length
      ? [`Questions with gold commits: ${tally((has) => has)} passed; the others: ${tally((has) => !has)}.`]
      : []),
  ];
lines.push(
  `Answer model: \`${options.model}\`; judge: \`${options["judge-model"]}\`; run ${new Date().toISOString()}.`,
  // A deployment's URL stays out of the repository.
  ...(options["mcp-url"] ? ["", "pghacker-mcp: the endpoint given as `--mcp-url`."] : []),
  "",
  ...(reviews ? reviewTable(results) : resultsTable(results)),
  "",
);
writeFileSync(path.resolve(config.root, options.results), lines.join("\n"));
console.log(`[eval] ${passed}/${results.length} passed. Details: ${path.join(dir, `eval-${stamp}.json`)}`);
if (passed !== results.length) process.exitCode = 1;
