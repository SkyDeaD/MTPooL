import { hydrate } from 'svelte';
import '../styles/index.css';
import NodePage from './NodePage.svelte';

// One static file serves every node; the node comes from ?id=.
const id = new URLSearchParams(location.search).get('id') ?? '';
hydrate(NodePage, { target: document.getElementById('app')!, props: { id } });
