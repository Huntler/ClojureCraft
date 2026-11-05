;; Evaluation harness for candidate transformation functions.
;;
;; Reads one EDN request from stdin:
;;   {:code "<source>" :fn "<public fn name>" :inputs ["<edn>" ...] :checks true}
;; and prints one EDN response to stdout.
;;
;; The static checks below are a second line of defence that produce clear diagnostics. The
;; operating-system sandbox around this JVM is the security boundary.

(ns clojurecraft.harness
  (:require [clojure.edn :as edn]
            [clojure.walk :as walk])
  (:import (java.io StringWriter)))

(def allowed-namespaces
  '#{clojure.core clojure.string clojure.set clojure.walk clojure.edn clojure.zip clojure.data})

;; Evaluation, reflection over namespaces and vars, state that escapes the call, concurrency,
;; class generation, and I/O.
(def denied-core
  '#{eval load load-file load-string load-reader read read-string read+string
     resolve ns-resolve requiring-resolve find-var intern var var-get var-set alter-var-root
     with-redefs with-redefs-fn binding with-bindings with-bindings* push-thread-bindings pop-thread-bindings
     set! ns-publics ns-interns ns-map ns-refers ns-aliases ns-imports all-ns find-ns the-ns
     create-ns remove-ns in-ns ns-unmap ns-unalias refer refer-clojure use require import alias
     future future-call pmap pcalls agent send send-off send-via await await-for shutdown-agents
     add-watch set-agent-send-executor! set-agent-send-off-executor! thread-bound?
     slurp spit file-seq line-seq with-open
     gen-class gen-interface proxy proxy-super reify deftype defrecord definterface
     extend extend-type extend-protocol memfn monitor-enter monitor-exit locking
     . .. new set-validator! add-tap tap>  *in* with-in-str})

(def allowed-classes '#{Math StrictMath Long Integer Double Character Boolean})

(def allowed-top-level '#{defn defn- def declare})

(defn- violation [msg form]
  (throw (ex-info msg {:phase :check :form (pr-str form)})))

(defn- check-symbol [aliases sym]
  (let [ns-part (namespace sym)
        name-part (name sym)]
    (cond
      (and (nil? ns-part) (contains? denied-core sym))
      (violation (str "use of " sym " is not allowed") sym)

      (and (nil? ns-part) (or (.startsWith name-part ".") (.endsWith name-part ".")))
      (violation (str "Java interop " sym " is not allowed") sym)

      (and (nil? ns-part) (.contains name-part ".") (not= name-part "."))
      (violation (str "class or namespace reference " sym " is not allowed") sym)

      (nil? ns-part) nil

      (contains? allowed-classes (symbol ns-part)) nil

      :else
      (let [target (get aliases (symbol ns-part) (symbol ns-part))]
        (cond
          (not (contains? allowed-namespaces target))
          (violation (str "namespace or class " ns-part " is not allowed") sym)

          (and (= target 'clojure.core) (contains? denied-core (symbol name-part)))
          (violation (str "use of " sym " is not allowed") sym))))))

(defn- check-body [aliases form]
  (walk/prewalk
   (fn [x]
     (cond
       (and (seq? x) (= 'quote (first x))) nil
       (symbol? x) (do (check-symbol aliases x) x)
       :else x))
   form))

(defn- ns-aliases-of
  "Validates the ns form and returns {alias namespace}."
  [form]
  (when-not (and (seq? form) (= 'ns (first form)) (symbol? (second form)))
    (violation "the first form must be an ns declaration" form))
  (reduce
   (fn [aliases clause]
     (cond
       (string? clause) aliases
       (and (seq? clause) (= :require (first clause)))
       (reduce (fn [acc spec]
                 (let [[lib & opts] (if (vector? spec) spec [spec])
                       opts (apply hash-map opts)]
                   (when-not (contains? allowed-namespaces lib)
                     (violation (str "requiring " lib " is not allowed") spec))
                   (when (and (:refer opts) (not (vector? (:refer opts))))
                     (violation ":refer :all is not allowed; use an alias or a vector of names" spec))
                   (cond-> acc (:as opts) (assoc (:as opts) lib))))
               aliases (rest clause))
       :else (violation "only :require clauses are allowed in ns" clause)))
   {} (drop 2 form)))

(defn read-all [code]
  (binding [*read-eval* false
            *default-data-reader-fn* nil]
    (let [rdr (clojure.lang.LineNumberingPushbackReader. (java.io.StringReader. code))
          eof (Object.)]
      (loop [forms []]
        (let [f (read {:eof eof :read-cond :allow :features #{:clj}} rdr)]
          (if (identical? f eof) forms (recur (conj forms f))))))))

(defn check-forms [forms]
  (let [aliases (ns-aliases-of (first forms))]
    (doseq [f (rest forms)]
      (when-not (and (seq? f) (contains? allowed-top-level (first f)))
        (violation "only defn, defn-, def and declare are allowed at top level" f))
      (check-body aliases f))
    (second (first forms))))

(defn- error-map [phase ^Throwable t]
  {:status :error
   :phase (or (:phase (ex-data t)) phase)
   :class (.getName (class t))
   :message (str (.getMessage t))
   :form (:form (ex-data t))})

(def max-captured-chars 65536)

(defn- bounded-writer
  "A writer that keeps at most `limit` characters and fails the call once it is exceeded."
  [^StringWriter sw limit]
  (proxy [java.io.Writer] []
    (write
      ([x]
       (let [s (if (string? x) x (if (integer? x) (str (char x)) (String. ^chars x)))]
         (.write this s 0 (count s))))
      ([x off len]
       (when (> (+ (.length (.getBuffer sw)) len) limit)
         (throw (ex-info "printed output exceeds the capture limit" {:phase :call})))
       (if (string? x)
         (.write sw ^String x (int off) (int len))
         (.write sw ^chars x (int off) (int len)))))
    (flush [])
    (close [])))

(defn- call-on [f input-text]
  (let [sw (StringWriter.)
        out (bounded-writer sw max-captured-chars)
        start (System/nanoTime)]
    (try
      (let [input (edn/read-string input-text)
            value (binding [*out* out] (f input))
            ;; Realize lazy results inside the measured call so errors surface here.
            printed (binding [*print-length* 100000 *print-level* 200] (pr-str value))]
        {:status :ok :value printed :out (str sw) :elapsed-ns (- (System/nanoTime) start)})
      (catch Throwable t
        (assoc (error-map :call t) :out (str sw) :elapsed-ns (- (System/nanoTime) start))))))

(defn run-request [{:keys [code fn inputs checks]}]
  (try
    (let [forms (read-all code)
          ns-sym (if checks
                   (check-forms forms)
                   (second (first forms)))]
      (binding [*ns* (create-ns 'clojurecraft.scratch)]
        (refer-clojure)
        (doseq [form forms] (eval form)))
      (let [v (ns-resolve (the-ns ns-sym) (symbol fn))]
        (if-not (and v (fn? @v))
          {:status :error :phase :load :message (str "no function named " fn)}
          {:status :ok :results (mapv #(call-on @v %) inputs)})))
    (catch Throwable t (error-map :load t))))

(let [request (edn/read-string (slurp *in*))
      response (run-request request)]
  (binding [*print-length* nil *print-level* nil]
    (prn response))
  (flush))
