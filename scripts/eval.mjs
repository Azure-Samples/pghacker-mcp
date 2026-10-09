import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { portalEnvironment, readLocalConfig, serviceUrl } from "./lib/config.mjs";
import { callTool } from "./lib/services.mjs";
import { appAgentNames, modelProvidersPath } from "./lib/worker.mjs";
import { lastJsonBlock, parseCsv, quoteIn } from "./lib/evalcheck.mjs";

// The wave 1 evaluation (npm start must be running):
//   npm run eval -- [--questions eval/questions.json] [--only q01,q02] [--concurrency 3]
// Each question goes to a pghacker-eval session, which may use only
// pghacker-mcp. The script checks every citation (the Message-ID exists and
// the quote is verbatim) and a judge session grades the answer against the
// gold answer. A question passes when it is judged correct and its citations
// are valid. Full results go to .runtime/eval; the summary to eval/RESULTS.md.
const { values: options } = parseArgs({
  options: {
    questions: { type: "string", default: "eval/questions.json" },
    only: { type: "string", default: "" },
    model: { type: "string", default: "github-copilot:gpt-6-luna" },
    "judge-model": { type: "string", default: "github-copilot:gpt-6-luna" },
    concurrency: { type: "string", default: "3" },
    "timeout-minutes": { type: "string", default: "20" },
  },
});

const config = readLocalConfig();
const mcp = serviceUrl(config.mcpPort);
const timeout = Number(options["timeout-minutes"]) * 60_000;

const sqlText = (value) => `'${String(value).replace(/'/g, "''")}'`;

/** Messages by Message-ID: body and thread. */
async function messages(ids) {
  if (!ids.length) return new Map();
  const csv = await callTool(mcp, "query", {
    sql: `SELECT json_agg(json_build_object('id', message_id, 'thread', thread_id, 'body', body)) FROM messages
          WHERE message_id IN (${ids.map(sqlText).join(", ")})`,
    max_rows: 10,
  });
  const rows = parseCsv(String(csv));
  const cell = rows[1]?.[0];
  const list = cell && cell !== "None" ? JSON.parse(cell) : [];
  return new Map(list.map((m) => [m.id, m]));
}

const JUDGE = `You grade answers about PostgreSQL's history against a gold answer written by an expert.
An answer is correct when it states the gold answer's key facts (it may add correct detail, and
may phrase them differently) and contradicts none of them. Reply with only a JSON object:
{"correct": true|false, "score": 0.0-1.0, "missing": "key facts missing or wrong", "reason": "one sentence"}`;

async function judge(client, question, answer) {
  const session = await client.createSession({ model: options["judge-model"], systemMessage: JUDGE });
  const reply = await session.sendAndWait(
    `Question: ${question.question}\n\nGold answer: ${question.gold_answer}\n\nAnswer to grade: ${answer}\n\nReply with the JSON object only.`,
    timeout,
  );
  const match = String(reply ?? "").match(/\{[\s\S]*\}/);
  try {
    return match ? JSON.parse(match[0]) : { correct: false, score: 0, reason: "the judge did not reply with JSON" };
  } catch {
    return { correct: false, score: 0, reason: "the judge's JSON did not parse" };
  }
}

async function evaluate(client, question) {
  const started = Date.now();
  const session = await client.createSessionForAgent("pghacker-eval", { model: options.model, title: `pghacker eval ${question.id}` });
  let reply;
  try {
    reply = await session.sendAndWait(question.question, timeout);
  } catch (error) {
    return { id: question.id, pass: false, error: error.message, session: session.sessionId };
  }
  const parsed = lastJsonBlock(reply);
  if (!parsed) return { id: question.id, pass: false, error: "no JSON answer block", reply, session: session.sessionId };
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
  const grade = await judge(client, question, parsed.answer ?? "");
  const needsCitations = goldIds.length > 0;
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
    pass: Boolean(grade.correct) && (needsCitations ? citationsValid : true),
    seconds: Math.round((Date.now() - started) / 1000),
    session: session.sessionId,
  };
}

const questions = JSON.parse(readFileSync(options.questions, "utf8"))
  .filter((q) => !options.only || options.only.split(",").includes(q.id));
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
      console.log(`[eval] ${result.id}: ${result.pass ? "PASS" : "FAIL"}` +
        `${result.error ? ` (${result.error})` : ` correct=${result.correct} citations_valid=${result.citations_valid}`}`);
    }
  });
  await Promise.all(workers);
} finally {
  await client.stop();
}

results.sort((a, b) => a.id.localeCompare(b.id));
const passed = results.filter((r) => r.pass).length;
const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const dir = path.join(config.runtimeDir, "eval");
mkdirSync(dir, { recursive: true });
writeFileSync(path.join(dir, `eval-${stamp}.json`), JSON.stringify({ options, results }, null, 2));
const lines = [
  "# Wave 1 evaluation",
  "",
  `${passed} of ${results.length} questions passed (judged correct, with valid verbatim citations).`,
  `Answer model: \`${options.model}\`; judge: \`${options["judge-model"]}\`; run ${new Date().toISOString()}.`,
  "",
  "| Question | Kind | Correct | Citations valid | On a gold thread | Seconds |",
  "| --- | --- | --- | --- | --- | --- |",
  ...results.map((r) => `| ${r.id} | ${r.kind ?? ""} | ${r.correct ? "yes" : "no"} | ${r.citations_valid ? "yes" : "no"} | ${r.on_gold_thread ? "yes" : "no"} | ${r.seconds ?? ""} |`),
  "",
];
writeFileSync(path.join(config.root, "eval", "RESULTS.md"), lines.join("\n"));
console.log(`[eval] ${passed}/${results.length} passed. Details: ${path.join(dir, `eval-${stamp}.json`)}`);
if (passed !== results.length) process.exitCode = 1;
