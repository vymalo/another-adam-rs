//! Skills at run time: the catalog in the prompt, `load_skill`, `read_skill_file` and
//! `preload_skills`, for embedded and directory manifests alike.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;
use std::sync::Arc;

use adam_agent_fixture::AGENT;
use adam_agent_fs::{Dir, ManifestSource, Strictness};
use adam_assembly::{AgentDef, Assembly, Error, LOAD_SKILL, READ_SKILL_FILE, SkillField};
use adam_llm_agent::{Conversation, ToolSet, user_message};
use adam_model::{Message, MockModel, ModelRequest, ToolCall};
use common::{instructions, runtime, spawn_worker, tools, wait_done, write};
use serde_json::{Value, json};

/// A directory with a root agent (`frontmatter` is its frontmatter), three skills and a subagent
/// with a skill of its own.
///
/// * `alpha` has three files: a text reference, a script and a binary asset.
/// * `beta` has none. `gamma` is a flat skill.
/// * `coder/helper` has `delta`, with one file.
fn tree(frontmatter: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            (
                "agent/instructions.md",
                &instructions(&format!("name: coder\n{frontmatter}"), "You are the coder."),
            ),
            (
                "agent/skills/alpha/SKILL.md",
                "---\nname: alpha\ndescription: Does alpha things. Use for alpha & <friends>.\n---\n\
                 Step 1 of alpha.\nSee [the guide](references/guide.md).\n",
            ),
            (
                "agent/skills/alpha/references/guide.md",
                "# Guide\nUse the guide.\n",
            ),
            ("agent/skills/alpha/scripts/run.sh", "echo alpha\n"),
            (
                "agent/skills/beta/SKILL.md",
                "---\nname: beta\ndescription: Does beta things.\n---\nStep 1 of beta.\n",
            ),
            (
                "agent/skills/gamma.md",
                "---\ndescription: Does gamma things.\n---\nStep 1 of gamma.\n",
            ),
            (
                "agent/subagents/helper/instructions.md",
                "---\ndescription: Helps.\n---\nYou help.\n",
            ),
            (
                "agent/subagents/helper/skills/delta/SKILL.md",
                "---\nname: delta\ndescription: Does delta things.\n---\nStep 1 of delta.\n",
            ),
            (
                "agent/subagents/helper/skills/delta/notes.txt",
                "delta notes",
            ),
        ],
    );
    let assets = dir.path().join("agent/skills/alpha/assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("logo.bin"), [0x89, 0x50, 0xff, 0x00]).unwrap();
    dir
}

fn source(dir: &tempfile::TempDir) -> Dir {
    Dir::new(dir.path()).default_name("coder")
}

fn def_of(dir: &tempfile::TempDir) -> AgentDef {
    AgentDef::from_source(&source(dir), Strictness::Lenient)
        .unwrap()
        .remove(0)
}

fn assemble(def: AgentDef, model: Arc<MockModel>) -> Assembly {
    def.bind(ToolSet::new()).unwrap().model(model, "m").unwrap()
}

fn info<'a>(assembly: &'a Assembly, name: &str) -> &'a adam_assembly::AgentInfo {
    assembly.info().iter().find(|i| i.name == name).unwrap()
}

fn call(id: &str, tool: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: tool.into(),
        arguments: args,
    }
}

/// The result of tool call `id` in the last request of the model, and whether it is an error.
fn result(request: &ModelRequest, id: &str) -> (String, bool) {
    request
        .messages
        .iter()
        .find_map(|m| match m {
            Message::Tool {
                call_id,
                content,
                is_error,
            } if call_id == id => Some((content.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// Run `agent` on a fresh runtime until it is done.
async fn run(assembly: &Assembly, agent: &str) -> adam_runtime::RunView {
    let rt = runtime(assembly);
    let worker = spawn_worker(&rt);
    let run = rt.start(agent, user_message("go"), None).await.unwrap();
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    view
}

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

fn tool<'a>(request: &'a ModelRequest, name: &str) -> &'a adam_model::ToolSpec {
    request.tools.iter().find(|t| t.name == name).unwrap()
}

// --- tier 1: the catalog ------------------------------------------------------------------

#[test]
fn the_catalog_of_the_fixture_matches_the_golden_file() {
    let fixture_tools = tools(&[
        "prepare_workspace",
        "run_checks",
        "ask_user",
        "linear__list_issues",
        "read_diff",
        "list_files",
        "fetch_page",
    ]);
    let assembly = AgentDef::from_manifest(AGENT)
        .unwrap()
        .bind(fixture_tools)
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    let got = assembly.info()[0].prompt.clone() + "\n";
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/coder-prompt.txt");
    if std::env::var_os("ADAM_UPDATE_GOLDEN").is_some() {
        std::fs::write(&golden, &got).unwrap();
    }
    let want = std::fs::read_to_string(&golden).unwrap();
    assert_eq!(got, want, "regenerate with ADAM_UPDATE_GOLDEN=1");
}

#[test]
fn the_catalog_follows_the_prompt_and_escapes_the_descriptions() {
    let dir = tree("skills: all");
    let assembly = assemble(def_of(&dir), Arc::new(MockModel::new()));
    let prompt = &info(&assembly, "coder").prompt;
    assert_eq!(
        prompt,
        "You are the coder.\n\n\
         The following skills provide specialized instructions for specific tasks.\n\
         When a task matches a skill's description, call the load_skill tool with the skill's name to load its full instructions.\n\
         Files a skill bundles are listed under <skill_resources> when it is loaded; read one with the read_skill_file tool.\n\
         <available_skills>\n\
         \x20 <skill>\n    <name>alpha</name>\n    <description>Does alpha things. Use for alpha &amp; &lt;friends&gt;.</description>\n  </skill>\n\
         \x20 <skill>\n    <name>beta</name>\n    <description>Does beta things.</description>\n  </skill>\n\
         \x20 <skill>\n    <name>gamma</name>\n    <description>Does gamma things.</description>\n  </skill>\n\
         </available_skills>"
    );
    assert_eq!(info(&assembly, "coder").skills, ["alpha", "beta", "gamma"]);
    assert_eq!(
        info(&assembly, "coder").tools,
        [LOAD_SKILL, READ_SKILL_FILE]
    );
}

#[test]
fn without_a_skill_there_is_no_catalog_and_no_tool() {
    // No `skills/` at all.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[(
            "agent/instructions.md",
            &instructions("name: coder", "You are the coder."),
        )],
    );
    let assembly = assemble(def_of(&dir), Arc::new(MockModel::new()));
    let plain = info(&assembly, "coder");
    assert_eq!(plain.prompt, "You are the coder.");
    assert!(plain.tools.is_empty() && plain.skills.is_empty());

    // Skills on disk, none selected.
    let dir = tree("skills: []");
    let assembly = assemble(def_of(&dir), Arc::new(MockModel::new()));
    let none = info(&assembly, "coder");
    assert_eq!(none.prompt, "You are the coder.");
    assert!(none.tools.is_empty() && none.skills.is_empty());
    // The subagent has its own selection, which is the default (all of its own).
    assert_eq!(info(&assembly, "coder/helper").skills, ["delta"]);
}

#[test]
fn a_skill_without_files_needs_no_reader() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("agent/instructions.md", "---\nname: coder\n---\nHi.\n"),
            (
                "agent/skills/only.md",
                "---\ndescription: Only one.\n---\nDo it.\n",
            ),
        ],
    );
    let assembly = assemble(def_of(&dir), Arc::new(MockModel::new()));
    let prompt = &info(&assembly, "coder").prompt;
    assert!(
        prompt.starts_with("Hi.\n\nThe following skills provide"),
        "{prompt}"
    );
    // No file to read anywhere: no reader, and the catalog does not mention it.
    assert!(!prompt.contains(READ_SKILL_FILE), "{prompt}");
    assert_eq!(info(&assembly, "coder").tools, [LOAD_SKILL]);
}

#[test]
fn skills_selects_a_subset_in_the_order_given() {
    let dir = tree("skills: [gamma, alpha, gamma]");
    let assembly = assemble(def_of(&dir), Arc::new(MockModel::new()));
    let coder = info(&assembly, "coder");
    assert_eq!(coder.skills, ["gamma", "alpha"]);
    let gamma = coder.prompt.find("<name>gamma</name>").unwrap();
    let alpha = coder.prompt.find("<name>alpha</name>").unwrap();
    assert!(gamma < alpha);
    assert!(!coder.prompt.contains("<name>beta</name>"));
}

// --- what the files get wrong ---------------------------------------------------------------

#[test]
fn a_skill_the_agent_does_not_have_is_a_startup_error() {
    let dir = tree("skills: [alph]");
    let error = def_of(&dir).bind(ToolSet::new()).unwrap_err();
    assert!(
        matches!(&error, Error::UnknownSkill { field: SkillField::Skills, suggestion: Some(s), .. } if s == "alpha"),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): `skills` names `alph`, which is not a skill of \
         this agent; did you mean `alpha`?; its skills: `alpha`, `beta`, `gamma`"
    );

    // A subagent's skills are its own: the parent's `alpha` is not `helper`'s.
    let dir = tree("");
    let helper = dir.path().join("agent/subagents/helper/instructions.md");
    std::fs::write(
        helper,
        "---\ndescription: Helps.\nskills: [alpha]\n---\nYou help.\n",
    )
    .unwrap();
    let error = def_of(&dir).bind(ToolSet::new()).unwrap_err();
    assert!(
        error.to_string().starts_with(
            "agent `coder/helper` (agent/subagents/helper/instructions.md): `skills` names `alpha`"
        ),
        "{error}"
    );

    let dir = tree("preload_skills: [nope]");
    let error = def_of(&dir).bind(ToolSet::new()).unwrap_err();
    assert!(
        matches!(
            &error,
            Error::UnknownSkill {
                field: SkillField::PreloadSkills,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        error.to_string().contains("`preload_skills` names `nope`"),
        "{error}"
    );
}

#[test]
fn a_preloaded_skill_must_be_selected() {
    let dir = tree("skills: [alpha]\npreload_skills: [beta]");
    let error = def_of(&dir).bind(ToolSet::new()).unwrap_err();
    assert!(matches!(error, Error::PreloadNotSelected { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): `preload_skills` names `beta`, which `skills` \
         does not select: add it to `skills` or remove it; selected: `alpha`"
    );
}

#[test]
fn a_registered_tool_cannot_take_the_name_of_a_skill_tool() {
    let dir = tree("skills: all");
    let error = def_of(&dir).bind(tools(&[LOAD_SKILL])).unwrap_err();
    assert!(
        matches!(&error, Error::ReservedToolName { tool, .. } if tool == LOAD_SKILL),
        "{error}"
    );
    // Without skills the name is free.
    let dir = tree("skills: []");
    assemble_with(def_of(&dir), tools(&[LOAD_SKILL]));
    // Left out of `tools:`, it does not collide either.
    let dir = tree("skills: all\ntools: []");
    assemble_with(def_of(&dir), tools(&[LOAD_SKILL]));
}

fn assemble_with(def: AgentDef, set: ToolSet) -> Assembly {
    def.bind(set)
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap()
}

#[test]
fn a_manifest_without_its_source_has_no_bytes_to_serve() {
    let dir = tree("skills: all");
    let package = source(&dir)
        .load()
        .unwrap()
        .into_package(Strictness::Lenient)
        .unwrap();
    let bare = || AgentDef::from_manifest(package.agents[0].clone()).unwrap();
    let error = bare().bind(ToolSet::new()).unwrap_err();
    assert!(
        matches!(error, Error::SkillFilesUnavailable { .. }),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("skill `alpha` bundles `assets/logo.bin`"),
        "{error}"
    );
    // A skill that bundles nothing needs none; a skill nobody selected is not asked for.
    let dir2 = tree("skills: [beta, gamma]");
    std::fs::remove_file(
        dir2.path()
            .join("agent/subagents/helper/skills/delta/notes.txt"),
    )
    .unwrap();
    let package2 = source(&dir2)
        .load()
        .unwrap()
        .into_package(Strictness::Lenient)
        .unwrap();
    AgentDef::from_manifest(package2.agents[0].clone())
        .unwrap()
        .bind(ToolSet::new())
        .unwrap();
    // With the source, the same manifest serves them.
    bare()
        .resources_from(&source(&dir))
        .unwrap()
        .bind(ToolSet::new())
        .unwrap();
}

#[test]
fn a_file_that_cannot_be_read_or_is_too_big_is_a_startup_error() {
    let dir = tree("skills: all");
    let package = source(&dir)
        .load()
        .unwrap()
        .into_package(Strictness::Lenient)
        .unwrap();
    std::fs::remove_file(dir.path().join("agent/skills/alpha/scripts/run.sh")).unwrap();
    let error = AgentDef::from_manifest(package.agents[0].clone())
        .unwrap()
        .resources_from(&source(&dir))
        .unwrap_err();
    assert!(matches!(error, Error::Manifest(_)), "{error}");

    let dir = tree("skills: all");
    std::fs::write(
        dir.path().join("agent/skills/alpha/references/big.md"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .unwrap();
    let error = AgentDef::from_source(&source(&dir), Strictness::Lenient).unwrap_err();
    assert!(
        matches!(&error, Error::SkillTooLarge { skill, limit, .. } if skill == "alpha" && *limit == 1024 * 1024),
        "{error}"
    );
    assert!(error.to_string().starts_with(
        "agent `coder` (agent/skills/alpha/SKILL.md): the files bundled with skill `alpha`"
    ));

    // The same at any depth: the origin is the subagent.
    let dir = tree("skills: all");
    std::fs::write(
        dir.path()
            .join("agent/subagents/helper/skills/delta/big.bin"),
        vec![0_u8; 1024 * 1024 + 1],
    )
    .unwrap();
    let error = AgentDef::from_source(&source(&dir), Strictness::Lenient).unwrap_err();
    assert!(
        error.to_string().starts_with("agent `coder/helper` ("),
        "{error}"
    );
}

// --- tiers 2 and 3, end to end ---------------------------------------------------------------

#[tokio::test]
async fn the_model_loads_a_skill_then_reads_one_of_its_files() {
    let dir = tree("skills: all");
    let model = Arc::new(MockModel::new());
    model.push_tool_calls(vec![call("c1", LOAD_SKILL, json!({"name": "alpha"}))]);
    model.push_tool_calls(vec![call(
        "c2",
        READ_SKILL_FILE,
        json!({"skill": "alpha", "path": "references/guide.md"}),
    )]);
    model.push_text("Done.");
    let assembly = assemble(def_of(&dir), model.clone());
    let view = run(&assembly, "coder").await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "Done.");

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0].system.as_deref(),
        Some(info(&assembly, "coder").prompt.as_str())
    );
    assert_eq!(tool_names(&requests[0]), [LOAD_SKILL, READ_SKILL_FILE]);

    // The schemas: an enum of what each tool takes.
    let load = tool(&requests[0], LOAD_SKILL);
    assert_eq!(
        load.parameters["properties"]["name"]["enum"],
        json!(["alpha", "beta", "gamma"])
    );
    assert_eq!(load.parameters["required"], json!(["name"]));
    let read = tool(&requests[0], READ_SKILL_FILE);
    assert_eq!(
        read.parameters["properties"]["skill"]["enum"],
        json!(["alpha"])
    );
    assert_eq!(read.parameters["required"], json!(["skill", "path"]));

    // Tier 2: the body, without its frontmatter, and the list of the files.
    let (loaded, is_error) = result(&requests[1], "c1");
    assert!(!is_error);
    assert_eq!(
        loaded,
        "<skill_content name=\"alpha\">\n\
         Step 1 of alpha.\nSee [the guide](references/guide.md).\n\n\
         Relative paths in this skill are relative to the skill's directory.\n\
         <skill_resources>\n  <file>assets/logo.bin</file>\n  <file>references/guide.md</file>\n  \
         <file>scripts/run.sh</file>\n</skill_resources>\n</skill_content>"
    );
    // Tier 3: the file.
    let (file, is_error) = result(&requests[2], "c2");
    assert!(!is_error);
    assert_eq!(file, "# Guide\nUse the guide.\n");

    // Both results are in the conversation the run keeps.
    let conversation: Conversation = serde_json::from_value(view.state).unwrap();
    let texts: Vec<String> = conversation.messages.iter().map(Message::text).collect();
    assert!(texts.contains(&loaded));
    assert!(texts.contains(&file));
}

#[tokio::test]
async fn the_refusals_tell_the_model_what_to_do() {
    let dir = tree("skills: [alpha, beta]");
    let model = Arc::new(MockModel::new());
    let read = |id: &str, skill: &str, path: &str| {
        call(id, READ_SKILL_FILE, json!({"skill": skill, "path": path}))
    };
    model.push_tool_calls(vec![
        call("unknown", LOAD_SKILL, json!({"name": "alph"})),
        call("unselected", LOAD_SKILL, json!({"name": "gamma"})),
        call("no-name", LOAD_SKILL, json!({"name": 7})),
        read("up", "alpha", "../x"),
        read("absolute", "alpha", "/etc/passwd"),
        read("deep-up", "alpha", "a/../../b"),
        read("not-listed", "alpha", "references/missing.md"),
        read("skill-md", "alpha", "SKILL.md"),
        read("binary", "alpha", "assets/logo.bin"),
        read("other-skill", "gamma", "references/guide.md"),
        read("no-files", "beta", "x"),
        read("empty", "alpha", ""),
        call("bad-args", READ_SKILL_FILE, json!({"skill": "alpha"})),
        read("dot", "alpha", "./references/guide.md"),
    ]);
    model.push_text("Done.");
    let assembly = assemble(def_of(&dir), model.clone());
    run(&assembly, "coder").await;
    let request = model.last_request().unwrap();
    let err = |id: &str| {
        let (text, is_error) = result(&request, id);
        assert!(is_error, "{id}: {text}");
        text
    };

    assert_eq!(
        err("unknown"),
        "no skill `alph` is available to you; did you mean `alpha`?; available: `alpha`, `beta`"
    );
    // `gamma` exists on disk, but `skills:` did not select it: same answer as for a made-up name.
    assert_eq!(
        err("unselected"),
        "no skill `gamma` is available to you; available: `alpha`, `beta`"
    );
    assert_eq!(
        err("no-name"),
        "`load_skill` needs a string argument `name`"
    );
    assert_eq!(
        err("up"),
        "`../x` leaves the skill's directory: `..` is not allowed"
    );
    assert!(err("absolute").contains("`/etc/passwd` is an absolute path"));
    assert!(err("deep-up").contains("`a/../../b` leaves the skill's directory"));
    assert_eq!(
        err("not-listed"),
        "skill `alpha` has no file `references/missing.md`; its files: `assets/logo.bin`, \
         `references/guide.md`, `scripts/run.sh`"
    );
    assert!(err("skill-md").ends_with("its instructions are loaded with load_skill"));
    assert_eq!(
        err("binary"),
        "`assets/logo.bin` of skill `alpha` is a binary file (4 bytes) and cannot be shown as text"
    );
    assert_eq!(
        err("other-skill"),
        "no skill `gamma` is available to you; available: `alpha`"
    );
    assert_eq!(
        err("no-files"),
        "skill `beta` has no file `x`; the skill bundles no files"
    );
    assert!(err("empty").starts_with("`path` is empty"));
    assert!(err("bad-args").starts_with("`read_skill_file` needs string arguments"));
    // A leading `./` is only a spelling.
    assert_eq!(
        result(&request, "dot"),
        ("# Guide\nUse the guide.\n".to_owned(), false)
    );
}

// --- preload_skills -------------------------------------------------------------------------

#[tokio::test]
async fn a_preloaded_skill_is_in_the_prompt_and_out_of_the_catalog() {
    let dir = tree("skills: all\npreload_skills: [alpha]");
    let model = Arc::new(MockModel::new());
    model.push_tool_calls(vec![
        call("again", LOAD_SKILL, json!({"name": "alpha"})),
        call(
            "file",
            READ_SKILL_FILE,
            json!({"skill": "alpha", "path": "scripts/run.sh"}),
        ),
        call("beta", LOAD_SKILL, json!({"name": "beta"})),
    ]);
    model.push_text("Done.");
    let assembly = assemble(def_of(&dir), model.clone());
    let coder = info(&assembly, "coder");
    assert_eq!(coder.skills, ["alpha", "beta", "gamma"]);
    assert_eq!(coder.preloaded, ["alpha"]);

    // The catalog lists what is left to load; the body of the preloaded skill follows it, with
    // its files.
    let (catalog, preloaded) = coder
        .prompt
        .split_once("</available_skills>")
        .expect("a catalog");
    assert!(!catalog.contains("<name>alpha</name>"), "{catalog}");
    assert!(catalog.contains("<name>beta</name>") && catalog.contains("<name>gamma</name>"));
    assert_eq!(
        preloaded,
        "\n\nThe following skills are already loaded. Follow them whenever a task matches their \
         description. Files a skill bundles are listed under <skill_resources>; read one with the \
         read_skill_file tool.\n\n\
         <skill_content name=\"alpha\">\nStep 1 of alpha.\nSee [the guide](references/guide.md).\n\n\
         Relative paths in this skill are relative to the skill's directory.\n\
         <skill_resources>\n  <file>assets/logo.bin</file>\n  <file>references/guide.md</file>\n  \
         <file>scripts/run.sh</file>\n</skill_resources>\n</skill_content>"
    );

    run(&assembly, "coder").await;
    let requests = model.requests();
    assert_eq!(
        tool(&requests[0], LOAD_SKILL).parameters["properties"]["name"]["enum"],
        json!(["beta", "gamma"])
    );
    // Its files stay readable; loading it again says there is nothing to load.
    let last = &requests[1];
    assert_eq!(
        result(last, "again"),
        (
            "skill `alpha` is already in your instructions; there is nothing to load".to_owned(),
            true
        )
    );
    assert_eq!(result(last, "file"), ("echo alpha\n".to_owned(), false));
    assert!(
        result(last, "beta")
            .0
            .starts_with("<skill_content name=\"beta\">")
    );
}

#[tokio::test]
async fn preloading_everything_leaves_no_loader_and_no_catalog() {
    let dir = tree("skills: [alpha, beta]\npreload_skills: [beta, alpha]");
    let model = Arc::new(MockModel::new());
    model.push_text("Done.");
    let assembly = assemble(def_of(&dir), model.clone());
    let coder = info(&assembly, "coder");
    assert_eq!(coder.preloaded, ["alpha", "beta"]);
    assert_eq!(coder.tools, [READ_SKILL_FILE]);
    assert!(!coder.prompt.contains("<available_skills>"));
    assert!(coder.prompt.contains("<skill_content name=\"alpha\">"));
    assert!(coder.prompt.contains("<skill_content name=\"beta\">"));
    run(&assembly, "coder").await;
    assert_eq!(tool_names(&model.requests()[0]), [READ_SKILL_FILE]);

    // Nothing to read either: no tool at all.
    let dir = tree("skills: [beta]\npreload_skills: [beta]");
    let assembly = assemble(def_of(&dir), Arc::new(MockModel::new()));
    let coder = info(&assembly, "coder");
    assert!(coder.tools.is_empty());
    assert!(
        coder
            .prompt
            .starts_with("You are the coder.\n\nThe following skills are already loaded.")
    );
    assert!(!coder.prompt.contains(READ_SKILL_FILE));
}

// --- subagents --------------------------------------------------------------------------------

#[tokio::test]
async fn a_subagent_has_its_own_skills() {
    let dir = tree("skills: [beta]");
    let model = Arc::new(MockModel::new());
    model.push_tool_calls(vec![
        call(
            "own",
            READ_SKILL_FILE,
            json!({"skill": "delta", "path": "notes.txt"}),
        ),
        // The parent's skill is not the child's.
        call("parents", LOAD_SKILL, json!({"name": "beta"})),
        call(
            "parents-file",
            READ_SKILL_FILE,
            json!({"skill": "alpha", "path": "scripts/run.sh"}),
        ),
    ]);
    model.push_text("Done.");
    let assembly = assemble(def_of(&dir), model.clone());

    let helper = info(&assembly, "coder/helper");
    assert_eq!(helper.skills, ["delta"]);
    assert_eq!(helper.tools, [LOAD_SKILL, READ_SKILL_FILE]);
    assert!(helper.prompt.contains("<name>delta</name>"));
    assert!(!helper.prompt.contains("<name>beta</name>"));
    assert_eq!(info(&assembly, "coder").skills, ["beta"]);

    run(&assembly, "coder/helper").await;
    let requests = model.requests();
    assert_eq!(
        tool(&requests[0], LOAD_SKILL).parameters["properties"]["name"]["enum"],
        json!(["delta"])
    );
    let last = &requests[1];
    assert_eq!(result(last, "own"), ("delta notes".to_owned(), false));
    assert_eq!(
        result(last, "parents"),
        (
            "no skill `beta` is available to you; available: `delta`".to_owned(),
            true
        )
    );
    assert!(result(last, "parents-file").1);
}

// --- embedded and directory ---------------------------------------------------------------------

#[tokio::test]
async fn embedded_and_directory_manifests_behave_the_same() {
    let fixture_tools = || {
        tools(&[
            "prepare_workspace",
            "run_checks",
            "ask_user",
            "linear__list_issues",
            "read_diff",
            "list_files",
            "fetch_page",
        ])
    };
    let script = |model: &MockModel| {
        model.push_tool_calls(vec![
            call("body", LOAD_SKILL, json!({"name": "release-notes"})),
            call(
                "style",
                READ_SKILL_FILE,
                json!({"skill": "release-notes", "path": "references/style.md"}),
            ),
            call(
                "script",
                READ_SKILL_FILE,
                json!({"skill": "release-notes", "path": "scripts/run.sh"}),
            ),
            call("flat", LOAD_SKILL, json!({"name": "triage"})),
            call("gone", LOAD_SKILL, json!({"name": "web-search"})),
            call(
                "up",
                READ_SKILL_FILE,
                json!({"skill": "release-notes", "path": "../../instructions.md"}),
            ),
        ]);
        model.push_text("Done.");
    };
    let dir = Dir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../adam-agent-fs/tests/fixtures/valid"
    ))
    .default_name("fixture");
    let from_dir = AgentDef::from_source(&dir, Strictness::Strict)
        .unwrap()
        .remove(0);
    let embedded = AgentDef::from_manifest(AGENT).unwrap();

    let mut seen: Vec<Vec<(String, bool)>> = Vec::new();
    let mut infos = Vec::new();
    for def in [embedded, from_dir] {
        let model = Arc::new(MockModel::new());
        script(&model);
        let assembly = def
            .bind(fixture_tools())
            .unwrap()
            .model(model.clone(), "m")
            .unwrap();
        run(&assembly, "coder").await;
        let last = model.last_request().unwrap();
        seen.push(
            ["body", "style", "script", "flat", "gone", "up"]
                .iter()
                .map(|id| result(&last, id))
                .collect(),
        );
        infos.push(assembly.info().to_vec());
    }
    assert_eq!(infos[0], infos[1]);
    assert_eq!(seen[0], seen[1]);

    let [body, style, script, flat, gone, up] = &seen[0][..] else {
        panic!("six results");
    };
    assert!(
        body.0
            .starts_with("<skill_content name=\"release-notes\">\n1. List the merged"),
        "{body:?}"
    );
    assert!(
        body.0
            .contains("<file>references/style.md</file>\n  <file>scripts/run.sh</file>")
    );
    assert_eq!(style, &("# Style\n".to_owned(), false));
    assert_eq!(script, &("echo run\n".to_owned(), false));
    assert_eq!(
        flat,
        &(
            "<skill_content name=\"triage\">\nRead the issue, then pick a queue.\n</skill_content>"
                .to_owned(),
            false
        )
    );
    // The researcher's skill is not the root's.
    assert!(
        gone.1 && gone.0.contains("available: `release-notes`, `triage`"),
        "{gone:?}"
    );
    assert!(
        up.1 && up.0.contains("leaves the skill's directory"),
        "{up:?}"
    );
}
