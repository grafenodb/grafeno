// Builds the retrieval fixtures of crates/grafeno-memory/data/recall:
//
// - longterm.json: thousands of conversations over two years, with old
//   decisions among them (some replaced later) and questions about those
//   decisions; made from a fixed seed, so it comes out the same each time;
// - corpus.vectors.json and longterm.vectors.json: the embeddings of every
//   memory and question, by a small multilingual embedding model run
//   locally (quantized, 384 numbers a vector, with "query: " and
//   "passage: " prefixes), as int8 with one scale per vector.
//
// The model is read from a local folder and nothing is downloaded:
//
//   node tools/fixtures/make-recall-fixtures.mjs <dir> <package> <models dir> <model id>
//
// where <dir> has <package>, a JavaScript library that runs such models
// (a feature-extraction pipeline), in its node_modules, and <models dir>
// holds the model <model id> in that library's layout.

import { createRequire } from "node:module";
import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const data = join(here, "..", "..", "crates", "grafeno-memory", "data", "recall");
const [packages, library, models, modelId] = process.argv.slice(2);
if (!packages || !library || !models || !modelId) {
  console.error("usage: make-recall-fixtures.mjs <dir> <package> <models dir> <model id>");
  process.exit(2);
}

// ---------------------------------------------------------------- the seed

let state = 0x9e3779b9;
function random() {
  // xorshift32: the same numbers every run.
  state ^= state << 13;
  state >>>= 0;
  state ^= state >>> 17;
  state ^= state << 5;
  state >>>= 0;
  return state / 0x100000000;
}
const pick = (items) => items[Math.floor(random() * items.length)];

// ------------------------------------------------- decisions and questions

// Each topic: what was decided first, what replaced it (if anything), and a
// question that names the topic in other words.
const topics = [
  ["es", "proveedor de correo transaccional", "Elegimos un proveedor de correo transaccional con servidores en Europa y límite de cien mil envíos al mes.", "Cambiamos el proveedor de correo transaccional por uno propio en nuestra nube, sin límite mensual.", "¿Con qué enviamos los emails automáticos a los clientes?"],
  ["es", "tamaño máximo de archivos subidos", "Los archivos que suben los usuarios no pueden pasar de veinte megas.", "El límite de los archivos subidos sube a cien megas para los planes de empresa.", "¿Hasta cuánto puede pesar lo que adjunta un usuario?"],
  ["es", "zona horaria de los informes", "Todos los informes se generan en hora de Madrid.", null, "¿En qué hora están las fechas de los reportes?"],
  ["es", "contraseñas de los usuarios", "Las contraseñas se guardan con un hash lento y sal por usuario; nunca en claro.", null, "¿Cómo almacenamos las claves de acceso de la gente?"],
  ["es", "idioma por defecto de la interfaz", "La interfaz arranca en el idioma del navegador y, si no lo hay, en español.", null, "¿Qué idioma ve alguien la primera vez que entra?"],
  ["es", "retención de registros de acceso", "Los registros de acceso se guardan seis meses y luego se borran.", "Los registros de acceso se guardan doce meses por la auditoría anual.", "¿Cuánto tiempo conservamos los logs de quién entra?"],
  ["es", "formato de fechas en la API", "La API devuelve las fechas en ISO 8601 con zona horaria.", null, "¿Cómo vienen las fechas en las respuestas del servicio?"],
  ["es", "paginación de listados", "Los listados de la API se paginan con cursor, nunca con número de página.", null, "¿Cómo se recorren las listas largas del servicio?"],
  ["es", "doble factor para administradores", "Las cuentas de administrador deben usar doble factor desde el primer día.", null, "¿Qué protección extra llevan las cuentas con más permisos?"],
  ["es", "proveedor de mapas", "Usamos un proveedor de mapas de pago por uso para las direcciones de entrega.", "Dejamos el proveedor de mapas de pago y usamos mapas abiertos alojados por nosotros.", "¿De dónde salen los mapas de las direcciones de envío?"],
  ["es", "horario del equipo de soporte", "El soporte atiende de ocho a ocho, de lunes a sábado.", null, "¿Cuándo hay alguien para atender a los clientes?"],
  ["es", "política de devoluciones", "Las devoluciones se aceptan durante treinta días con el ticket.", "Las devoluciones se aceptan durante cuarenta y cinco días, con o sin ticket.", "¿Hasta cuándo puede alguien devolver una compra?"],
  ["es", "base de datos de sesiones", "Las sesiones se guardan en un almacén en memoria con caducidad de dos horas.", null, "¿Dónde viven las sesiones abiertas y cuánto duran?"],
  ["es", "nombres de las ramas", "Las ramas se llaman tipo/descripción-corta, por ejemplo arreglo/login-lento.", null, "¿Cómo se nombran las ramas de trabajo?"],
  ["es", "versión mínima del navegador", "La web soporta las dos últimas versiones de cada navegador principal.", null, "¿Qué navegadores antiguos dejamos de soportar?"],
  ["es", "frecuencia de las encuestas a clientes", "Enviamos una encuesta de satisfacción a cada cliente como mucho una vez por trimestre.", null, "¿Cada cuánto preguntamos a los clientes qué tal les va?"],
  ["es", "moneda de los precios internos", "Los precios internos se guardan en céntimos de euro, como enteros.", null, "¿Cómo se representan los importes en la base de datos?"],
  ["es", "copias del código", "El repositorio se replica cada noche en un segundo proveedor.", null, "¿Qué pasa si perdemos el alojamiento del código?"],
  ["es", "acceso a producción", "Nadie entra a producción por consola sin una petición aprobada y registrada.", "El acceso por consola a producción se da por una hora con aprobación de dos personas.", "¿Cómo consigue alguien entrar a las máquinas reales?"],
  ["es", "notificaciones por SMS", "Los SMS solo se usan para códigos de verificación, nunca para publicidad.", null, "¿Para qué mandamos mensajes de texto al móvil?"],
  ["en", "on-call compensation", "On-call weeks are paid as an extra day off for each weekend covered.", "On-call is now paid as a fixed weekly bonus, plus a day off after any night page.", "How are people rewarded for being on call?"],
  ["en", "design system", "All new screens use the shared design system components; custom ones need a design review.", null, "Can I build my own button for a new page?"],
  ["en", "error tracking", "Errors in production are reported to the hosted error tracker with the release version attached.", "Error reports go to our self-hosted tracker; the hosted one was turned off for cost.", "Where do production exceptions get reported?"],
  ["en", "meeting-free day", "Wednesdays have no meetings so people can focus.", null, "Is there a day without calls on the calendar?"],
  ["en", "test coverage gate", "Pull requests may not lower the line coverage of the files they touch.", null, "Will my change be blocked if it reduces test coverage?"],
  ["en", "API versioning", "The public API is versioned in the path; a version is supported for eighteen months after the next one.", null, "How long do we keep an old version of the public API running?"],
  ["en", "office hours for interns", "Interns get a weekly hour with a senior engineer of their choice.", null, "Who mentors the interns and how often?"],
  ["en", "laptop policy", "Engineers get a new laptop every three years, or sooner if it fails.", "Laptops are replaced every four years; a failed one is repaired first.", "How often is someone's work computer replaced?"],
  ["en", "code freeze", "There is a code freeze the last week of December; only fixes ship.", null, "Can we release new features right before the new year?"],
  ["en", "data residency", "Customer data of European accounts stays in European regions.", null, "Where is a European customer's data stored?"],
  ["en", "incident severity", "An incident is severity one when payments or sign-in fail for more than five percent of users.", null, "What counts as the worst kind of outage?"],
  ["en", "dependency updates", "Dependencies are updated in a batch every first Monday of the month.", "Dependency updates are opened automatically each week and merged when the checks pass.", "When do we bump library versions?"],
  ["en", "remote work", "People may work remotely three days a week.", "Work is fully remote; the office is for team days once a month.", "How many days do we have to be in the office?"],
  ["en", "release notes", "Every release has notes written for customers, not for engineers.", null, "Who reads the notes that go with a new version?"],
  ["en", "feature requests", "Customer feature requests are logged in the tracker with the account name and voted on monthly.", null, "What happens when a customer asks for something new?"],
  ["en", "staging data", "Staging uses generated data only; production data never leaves production.", null, "Can I copy real customer records to the test environment?"],
  ["en", "password reset links", "Password reset links expire after thirty minutes and work once.", null, "How long is the link to reset my password valid?"],
  ["en", "image formats", "Product images are stored as WebP with a JPEG fallback for old clients.", "Product images are stored as AVIF, with WebP for clients that cannot read it.", "Which file type do product pictures use?"],
  ["en", "accessibility target", "Every public page must meet the WCAG AA level.", null, "How accessible do our public pages need to be?"],
  ["en", "hiring loop", "Candidates meet four interviewers in one day, and the decision is made within two days.", null, "How long does a candidate wait to hear back after interviews?"]
];

const chatter = {
  es: [
    "En la reunión de hoy repasamos los pendientes de {project} y quedó todo como estaba.",
    "Alguien preguntó por {topic}; dijimos que lo veríamos otro día, sin decidir nada.",
    "Revisamos un error menor en {project}: un texto mal traducido en la pantalla de ajustes.",
    "Se habló de {topic} en el canal general, pero era una duda de un cliente.",
    "Hoy {person} estuvo de vacaciones y la revisión de {project} se movió al jueves.",
    "Pequeño fallo en {project}: un botón no respondía en pantallas muy estrechas; corregido.",
    "Planificamos la semana de {project}; nada nuevo sobre {topic}.",
    "Un cliente pidió una factura duplicada; soporte la reenvió desde el panel.",
    "Se actualizó la documentación interna de {project} con capturas nuevas.",
    "Conversación larga sobre nombres de variables en {project}; se dejó a criterio de cada uno."
  ],
  en: [
    "Today's standup went over {project}'s open tasks; nothing changed.",
    "Someone asked about {topic}; we said we would look at it later, nothing decided.",
    "We looked at a minor bug in {project}: a label overflowed on small screens.",
    "{topic} came up in the general channel, but it was a customer's question.",
    "{person} was out today, so the {project} review moved to Thursday.",
    "Small fix in {project}: a tooltip showed the wrong date format; fixed.",
    "We planned the week for {project}; nothing new about {topic}.",
    "A customer asked for a copy of an invoice; support resent it.",
    "The internal docs of {project} got new screenshots.",
    "Long discussion about naming in {project}; left to each person."
  ]
};
const projects = ["facturación", "la app móvil", "el pipeline de datos", "la web", "infraestructura", "billing", "the mobile app", "the data pipeline", "the website", "infrastructure"];
const people = ["Lucía", "Priya", "Marta", "Tomás", "Diego", "Elena", "Sara", "Andrés"];

const start = Date.UTC(2025, 0, 6);
const day = 86_400_000;
function at(days) {
  return new Date(start + Math.floor(days * day)).toISOString();
}

const conversations = [];
const CONVERSATIONS = 3000;
for (let i = 0; i < CONVERSATIONS; i++) {
  const lang = random() < 0.55 ? "es" : "en";
  const topic = pick(topics.filter((t) => t[0] === lang))[1];
  const text = pick(chatter[lang])
    .replaceAll("{project}", pick(projects))
    .replaceAll("{topic}", topic)
    .replaceAll("{person}", pick(people));
  conversations.push({ key: `conv-${String(i).padStart(4, "0")}`, at: at((i / CONVERSATIONS) * 720), text });
}

const decisions = [];
const questions = [];
topics.forEach(([, , first, later, question], index) => {
  const key = `dec-${String(index).padStart(2, "0")}`;
  // Old decisions: in the first three months.
  decisions.push({ key, at: at(5 + index * 2), text: first });
  if (later) {
    decisions.push({ key: `${key}-new`, at: at(400 + index * 7), text: later, supersedes: key });
    questions.push({ q: question, kind: "replaced", answer: [`${key}-new`], replaced: key });
  } else {
    questions.push({ q: question, kind: "old", answer: [key] });
  }
});

const longterm = {
  about: "Thousands of conversations over two years, with decisions made in the first months (some replaced later, a `supersedes` relation from the new one) and a question about each. Generated by tools/fixtures/make-recall-fixtures.mjs from a fixed seed.",
  conversations,
  decisions,
  questions
};
writeFileSync(join(data, "longterm.json"), JSON.stringify(longterm, null, 1) + "\n");

// --------------------------------------------------------------- embedding

const require = createRequire(join(packages, "package.json"));
const transformers = await import(pathToFileURL(require.resolve(library)).href);
transformers.env.allowRemoteModels = false;
transformers.env.localModelPath = models;
transformers.env.cacheDir = models;
const embed = await transformers.pipeline("feature-extraction", modelId, { dtype: "q8" });

async function vectors(items, prefix) {
  const out = [];
  for (let i = 0; i < items.length; i += 32) {
    const batch = items.slice(i, i + 32);
    const tensor = await embed(batch.map((t) => `${prefix}: ${t}`), { pooling: "mean", normalize: true });
    const [rows, dims] = tensor.dims;
    for (let r = 0; r < rows; r++) {
      const v = Array.from(tensor.data.slice(r * dims, (r + 1) * dims));
      const scale = Math.max(...v.map(Math.abs)) / 127 || 1;
      const bytes = Int8Array.from(v.map((x) => Math.round(x / scale)));
      out.push({ s: scale, e: Buffer.from(bytes.buffer).toString("base64") });
    }
    process.stderr.write(`\r${prefix}: ${Math.min(i + 32, items.length)}/${items.length}`);
  }
  process.stderr.write("\n");
  return out;
}

const corpus = JSON.parse(readFileSync(join(data, "corpus.json"), "utf8"));
const corpusVectors = {
  model: "a small multilingual embedding model (q8, 384 dimensions), mean pooled, normalized; int8 with a scale per vector",
  memories: await vectors(corpus.memories.map((m) => m.text), "passage"),
  questions: await vectors(corpus.questions.map((q) => q.q), "query")
};
writeFileSync(join(data, "corpus.vectors.json"), JSON.stringify(corpusVectors) + "\n");

const longtermVectors = {
  model: corpusVectors.model,
  conversations: await vectors(conversations.map((c) => c.text), "passage"),
  decisions: await vectors(decisions.map((d) => d.text), "passage"),
  questions: await vectors(questions.map((q) => q.q), "query")
};
writeFileSync(join(data, "longterm.vectors.json"), JSON.stringify(longtermVectors) + "\n");
console.log(`${corpus.memories.length} memories, ${corpus.questions.length} questions; ${conversations.length} conversations, ${decisions.length} decisions, ${questions.length} questions`);
