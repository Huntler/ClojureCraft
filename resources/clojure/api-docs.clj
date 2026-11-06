;; Prints the API documentation of the supported standard-library namespaces as JSON.
;;
;; Reads the metadata of every public, documented var (name, arglists, docstring, version added,
;; macro and deprecation flags) and the special-form documentation from the running Clojure, so
;; the result always describes exactly the installed Clojure version. Takes no input.

(ns clojurecraft.api-docs
  (:require [clojure.string :as str]
            [clojure.repl]))

(def namespaces
  '[clojure.core clojure.string clojure.set clojure.walk clojure.edn clojure.zip clojure.data])

(defn- json-string [^String s]
  (let [sb (StringBuilder. "\"")]
    (doseq [c s]
      (case c
        \" (.append sb "\\\"")
        \\ (.append sb "\\\\")
        \newline (.append sb "\\n")
        \return (.append sb "\\r")
        \tab (.append sb "\\t")
        (if (< (int c) 0x20)
          (.append sb (format "\\u%04x" (int c)))
          (.append sb c))))
    (.append sb "\"")
    (str sb)))

(defn- json [x]
  (cond
    (nil? x) "null"
    (true? x) "true"
    (false? x) "false"
    (string? x) (json-string x)
    (keyword? x) (json-string (name x))
    (number? x) (str x)
    (map? x) (str "{" (str/join "," (map (fn [[k v]] (str (json-string (name k)) ":" (json v))) x)) "}")
    (sequential? x) (str "[" (str/join "," (map json x)) "]")
    :else (json-string (str x))))

(defn- arglists [m]
  (mapv #(binding [*print-length* nil *print-level* nil] (pr-str %)) (:arglists m)))

(defn- var-entry [ns-sym [sym v]]
  (let [m (meta v)]
    (when (and (:doc m) (not (:private m)))
      {:ns (str ns-sym)
       :name (str sym)
       :kind (cond (:macro m) "macro" (fn? @v) "function" :else "var")
       :arglists (arglists m)
       :doc (:doc m)
       :added (:added m)
       :deprecated (some-> (:deprecated m) str)
       :dynamic (boolean (:dynamic m))})))

(defn- special-forms []
  (for [[sym m] (sort-by (comp str key) @#'clojure.repl/special-doc-map)]
    {:ns "clojure.core"
     :name (str sym)
     :kind "special-form"
     :arglists (mapv pr-str (:forms m))
     :doc (:doc m)
     :added nil
     :deprecated nil
     :dynamic false}))

(doseq [n namespaces] (require n))

(let [vars (for [n namespaces
                 entry (sort-by (comp str key) (ns-publics n))
                 :let [e (var-entry n entry)]
                 :when e]
             e)]
  (println (json {:clojure_version (clojure-version)
                  :entries (vec (concat (special-forms) vars))}))
  (flush))
