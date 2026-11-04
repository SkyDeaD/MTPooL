import { mount } from 'svelte';
import '../styles/index.css';
import App from './App.svelte';

mount(App, { target: document.getElementById('app')! });
