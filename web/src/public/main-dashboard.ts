import { hydrate } from 'svelte';
import '../styles/index.css';
import Dashboard from './Dashboard.svelte';

hydrate(Dashboard, { target: document.getElementById('app')! });
