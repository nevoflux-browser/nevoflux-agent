"use strict";
// 中文商店的商品目录。任务答案由这里的数据推出——改价格要同步改对应任务 JSON。
window.SITE = "zh-shop";
window.L10N = {
  currency: "¥", search: "搜索", searchPlaceholder: "搜索商品",
  results: "搜索结果", page: "第", next: "下一页", prev: "上一页",
  addToCart: "加入购物车", qty: "数量", cart: "购物车", checkout: "去结算",
  emptyCart: "购物车是空的", name: "收货人", address: "收货地址",
  delivery: "配送方式", standard: "普通配送", express: "快递配送",
  placeOrder: "提交订单", ordered: "下单成功", rating: "评分",
  reviews: "条评价", remove: "删除", warranty: "保修", months: "个月", added: "已加入购物车",
  featured: "全部商品", noResults: "没有找到商品",
};
window.PRODUCTS = [
  { sku: "Z-1", name: "青轴机械键盘 87键", price: 299, rating: 4.3, reviews: 860, warranty: 12 },
  { sku: "Z-2", name: "红轴机械键盘 104键", price: 359, rating: 4.6, reviews: 420, warranty: 24 },
  { sku: "Z-3", name: "茶轴机械键盘 无线版", price: 429, rating: 4.1, reviews: 95, warranty: 24 },
  { sku: "Z-4", name: "静音机械键盘 68键", price: 259, rating: 4.8, reviews: 132, warranty: 12 },
  { sku: "Z-5", name: "RGB 机械键盘 电竞版", price: 499, rating: 3.9, reviews: 1250, warranty: 36 },
  { sku: "Z-6", name: "矮轴机械键盘 超薄", price: 239, rating: 3.5, reviews: 64, warranty: 12 },
  { sku: "Z-7", name: "薄膜键盘 办公款", price: 89, rating: 4.0, reviews: 300, warranty: 12 },
  { sku: "Z-8", name: "人体工学键盘", price: 329, rating: 4.4, reviews: 77, warranty: 24 },
  { sku: "Z-9", name: "无线鼠标 静音款", price: 79, rating: 4.2, reviews: 540, warranty: 12 },
  { sku: "Z-10", name: "游戏鼠标 有线", price: 149, rating: 4.5, reviews: 210, warranty: 12 },
  { sku: "Z-11", name: "键盘手托 记忆棉", price: 49, rating: 4.7, reviews: 45, warranty: 6 },
  { sku: "Z-12", name: "USB 扩展坞", price: 199, rating: 3.3, reviews: 30, warranty: 12 },
];
