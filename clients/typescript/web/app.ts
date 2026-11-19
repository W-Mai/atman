import { mount } from "svelte";
import Workbench from "./Workbench.svelte";
import { bindWorkbench } from "./workspace.ts";
import "./app.css";

const target = document.querySelector("#app");
if (target === null) throw new Error("Missing workbench mount point");
mount(Workbench, { target, props: { onReady: bindWorkbench } });
