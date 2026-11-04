import { hydrate } from 'svelte';
import '../styles/index.css';
import Links from './Links.svelte';

hydrate(Links, { target: document.getElementById('app')! });
