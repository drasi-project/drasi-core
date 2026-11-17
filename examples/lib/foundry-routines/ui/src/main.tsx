import React from 'react';
import ReactDOM from 'react-dom/client';
import { FluentProvider, webDarkTheme } from '@fluentui/react-components';
import App from './App';
import './styles.css';

const theme = {
  ...webDarkTheme,
  colorBrandBackground: '#8151dc',
  colorBrandBackgroundHover: '#9060ed',
  colorBrandBackgroundPressed: '#7141cc',
  colorBrandForeground1: '#b99aff',
  colorBrandForeground2: '#b99aff',
  colorBrandStroke1: '#9b6dff',
  colorCompoundBrandBackground: '#9b6dff',
  colorCompoundBrandBackgroundHover: '#ad87ff',
  colorCompoundBrandBackgroundPressed: '#8151dc',
  colorCompoundBrandForeground1: '#b99aff',
  colorCompoundBrandStroke: '#9b6dff',
  colorNeutralBackground1: '#202020',
  colorNeutralBackground2: '#171717',
};

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode><FluentProvider theme={theme}><App /></FluentProvider></React.StrictMode>,
);
