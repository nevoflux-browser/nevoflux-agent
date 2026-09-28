"use strict";
// Catalogue for the English shop. Task answers are derived from these rows —
// change a price here and the matching task JSON must change too.
window.SITE = "shop";
window.L10N = {
  currency: "$", search: "Search", searchPlaceholder: "Search products",
  results: "Results for", page: "Page", next: "Next page", prev: "Previous page",
  addToCart: "Add to cart", qty: "Quantity", cart: "Cart", checkout: "Checkout",
  emptyCart: "Your cart is empty", name: "Full name", address: "Address",
  delivery: "Delivery", standard: "Standard delivery", express: "Express delivery",
  placeOrder: "Place order", ordered: "Order placed", rating: "Rating",
  reviews: "reviews", remove: "Remove", warranty: "Warranty", months: "months", added: "Added to cart",
  featured: "All products", noResults: "No products found",
};
window.PRODUCTS = [
  { sku: "K-1", name: "Aster Mechanical Keyboard", price: 89.0, rating: 4.2, reviews: 312, warranty: 12 },
  { sku: "K-2", name: "Borealis Low-Profile Keyboard", price: 64.5, rating: 4.6, reviews: 158, warranty: 24 },
  { sku: "K-3", name: "Cobalt 60% Keyboard", price: 49.99, rating: 3.9, reviews: 97, warranty: 36 },
  { sku: "K-4", name: "Dune Wireless Keyboard", price: 72.0, rating: 2.8, reviews: 41, warranty: 12 },
  { sku: "K-5", name: "Ember Gaming Keyboard", price: 129.0, rating: 4.4, reviews: 520, warranty: 24 },
  { sku: "K-6", name: "Fjord Compact Keyboard", price: 55.25, rating: 4.8, reviews: 76, warranty: 12 },
  { sku: "M-1", name: "Glide Optical Mouse", price: 19.99, rating: 4.1, reviews: 230, warranty: 12 },
  { sku: "M-2", name: "Harbor Ergonomic Mouse", price: 39.0, rating: 4.5, reviews: 180, warranty: 24 },
  { sku: "M-3", name: "Iris Travel Mouse", price: 24.5, rating: 2.5, reviews: 33, warranty: 12 },
  { sku: "M-4", name: "Juniper Gaming Mouse", price: 59.9, rating: 4.3, reviews: 410, warranty: 24 },
  { sku: "M-5", name: "Kestrel Silent Mouse", price: 29.0, rating: 3.7, reviews: 88, warranty: 12 },
  { sku: "M-6", name: "Lumen Trackball", price: 69.0, rating: 2.9, reviews: 52, warranty: 24 },
  { sku: "H-1", name: "Meadow Headset", price: 45.0, rating: 4.0, reviews: 140, warranty: 12 },
  { sku: "H-2", name: "Nimbus Wireless Headset", price: 99.0, rating: 4.7, reviews: 265, warranty: 24 },
  { sku: "H-3", name: "Orbit USB Headset", price: 35.5, rating: 2.7, reviews: 61, warranty: 12 },
  { sku: "H-4", name: "Prism Studio Headphones", price: 149.0, rating: 4.9, reviews: 190, warranty: 36 },
  { sku: "P-1", name: "Quill Mouse Pad", price: 12.0, rating: 4.4, reviews: 510, warranty: 6 },
  { sku: "P-2", name: "Relay USB Hub", price: 27.75, rating: 3.2, reviews: 120, warranty: 12 },
  { sku: "P-3", name: "Slate Laptop Stand", price: 42.0, rating: 4.6, reviews: 205, warranty: 24 },
  { sku: "P-4", name: "Tidal Webcam", price: 58.0, rating: 2.6, reviews: 44, warranty: 12 },
];
