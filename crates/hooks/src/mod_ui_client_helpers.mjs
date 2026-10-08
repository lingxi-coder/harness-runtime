const Fragment = Symbol.for('claude-code.surface.Fragment');

function h(type, props, ...children) {
  const elementProps = props == null ? {} : { ...props };
  const childList = children.length > 0
    ? children
    : elementProps.children === undefined
      ? []
      : Array.isArray(elementProps.children) ? elementProps.children : [elementProps.children];
  delete elementProps.children;
  return {
    type: type === Fragment ? 'Fragment' : type,
    props: elementProps,
    children: childList,
  };
}

const Box = 'Box';
const Text = 'Text';
const Button = 'Button';
const Input = 'Input';
const Select = 'Select';
const Link = 'Link';
const Code = 'Code';
const Markdown = 'Markdown';

export { h, Fragment, Box, Text, Button, Input, Select, Link, Code, Markdown };
